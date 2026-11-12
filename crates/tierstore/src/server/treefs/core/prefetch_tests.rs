//! Prefetch: an attach that asks for it fills and pins every blob of the tree in the background.

use std::sync::Arc;

use super::BATCH;
use crate::server::treefs::testing::{
    Built, FakeKernel, Rig, file, open_cached, sample, sha, until,
};

const RUN: &str = "run-1";

/// The blobs the fills asked for, in order (the manifests, which have no size, are left out).
fn blob_fills(rig: &Rig) -> Vec<String> {
    rig.upstream
        .asked("fill")
        .iter()
        .filter(|line| line.contains(" from "))
        .map(|line| line.split(' ').nth(1).unwrap().to_owned())
        .collect()
}

/// The blobs the pin requests named, in order.
fn pinned(rig: &Rig) -> Vec<String> {
    rig.upstream
        .asked("pin")
        .iter()
        .flat_map(|line| {
            line.split(' ')
                .skip(3)
                .map(str::to_owned)
                .collect::<Vec<_>>()
        })
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
    assert!(pinned(&rig).is_empty());
    rig.upstream.release_fills();
    until(|| rig.core.prefetching() == 0);
    assert_eq!(blob_fills(&rig), sample_blobs());
    let mut named = pinned(&rig);
    named.sort();
    let mut want = sample_blobs();
    want.sort();
    assert_eq!(named, want);
    let (fills, pins) = (blob_fills(&rig).len(), rig.upstream.asked("pin").len());
    let _ = open_cached(&rig.core, crate::server::treefs::ino::of(1, 3), false).unwrap();
    assert_eq!(blob_fills(&rig).len(), fills);
    assert_eq!(rig.upstream.asked("pin").len(), pins);
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
    assert!(!pinned(&rig).contains(&tool));
    assert_eq!(pinned(&rig).len(), 3);
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
    assert!(pinned(&rig).is_empty());
}

/// Asserts that a prefetch goes on when the last run detaches while a lookup has the tree attached
/// with no run: with a fill paused mid-prefetch, the run's detach unpins nothing, and once the fill
/// goes on every blob of the tree is filled and pinned for it, the implicit attachment still there.
#[test]
fn a_prefetch_goes_on_after_the_last_run_detaches_while_a_lookup_has_the_tree() {
    let rig = Rig::new("tfs-prefetch-implicit", sample());
    let digest = root(&rig);
    assert!(matches!(
        rig.core.lookup(crate::server::treefs::ino::TREES, &digest),
        Ok(super::super::Looked::Found(_))
    ));
    assert_eq!(rig.core.implicit(), vec![1]);
    rig.upstream.hold_fills();
    let _ = rig.core.attach_asking(&digest, RUN, true).unwrap();
    until(|| !blob_fills(&rig).is_empty());
    assert!(rig.core.detach(RUN).is_some());
    assert!(rig.upstream.asked("unpin").is_empty());
    rig.upstream.release_fills();
    until(|| rig.core.prefetching() == 0);
    assert_eq!(blob_fills(&rig), sample_blobs());
    let mut named = pinned(&rig);
    named.sort();
    let mut want = sample_blobs();
    want.sort();
    assert_eq!(named, want);
    assert!(
        rig.upstream
            .asked("pin")
            .iter()
            .all(|line| line.starts_with(&format!("pin mount-1 {digest} ")))
    );
    assert_eq!(rig.core.implicit(), vec![1]);
    assert!(rig.upstream.asked("unpin").is_empty());
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
    assert!(rig.upstream.asked("pin").is_empty());
    assert!(rig.upstream.asked("touch").is_empty());
}

/// Asserts that blobs are pinned in requests of at most `BATCH`, the last one with the rest.
#[test]
fn pins_go_in_batches() {
    let files: Vec<(String, Vec<u8>)> = (0..BATCH + 10)
        .map(|i| (format!("f{i:04}"), format!("contents {i}").into_bytes()))
        .collect();
    let built = Built::of(files.iter().map(|(n, c)| (n.as_str(), file(c))).collect());
    let rig = Rig::new("tfs-prefetch-batches", built);
    let _ = rig.core.attach_asking(&root(&rig), RUN, true).unwrap();
    until(|| rig.core.prefetching() == 0);
    let sizes: Vec<usize> = rig
        .upstream
        .asked("pin")
        .iter()
        .map(|line| line.split(' ').count() - 3)
        .collect();
    assert_eq!(sizes, [BATCH, 10]);
}

/// Asserts that a pin the cache refuses leaves the blobs unpinned, so that the open of one asks
/// again, and that a prefetch of a tree already pinned fills and pins nothing more.
#[test]
fn a_refused_pin_is_asked_again_by_an_open_and_a_pinned_tree_is_not_prefetched_twice() {
    let rig = Rig::new("tfs-prefetch-refused", sample());
    *rig.upstream.pin_answer.lock().unwrap() = Some("refused over pin_limit".to_owned());
    let _ = rig.core.attach_asking(&root(&rig), RUN, true).unwrap();
    until(|| rig.core.prefetching() == 0);
    let pins = rig.upstream.asked("pin").len();
    let _ = open_cached(&rig.core, crate::server::treefs::ino::of(1, 4), false).unwrap();
    assert_eq!(rig.upstream.asked("pin").len(), pins + 1);
    *rig.upstream.pin_answer.lock().unwrap() = None;
    rig.core.prefetch_here(1);
    assert_eq!(pinned(&rig).len(), 4 + 1 + 4);
    let (fills, pins) = (blob_fills(&rig).len(), rig.upstream.asked("pin").len());
    rig.core.prefetch_here(1);
    assert_eq!(
        (blob_fills(&rig).len(), rig.upstream.asked("pin").len()),
        (fills, pins)
    );
}

/// Asserts that a prefetch of a tree that is not there does nothing, and that blobs already in
/// the cache are pinned without a fill.
#[test]
fn a_missing_tree_is_not_prefetched_and_cached_blobs_are_not_filled() {
    let rig = Rig::new("tfs-prefetch-cached", sample());
    rig.core.prefetch_here(77);
    assert!(rig.upstream.asked("pin").is_empty());
    let _ = rig.core.attach(&root(&rig), RUN).unwrap();
    for blob in sample_blobs() {
        let bytes = rig.upstream.built.lock().unwrap().blobs[&blob].clone();
        let path = crate::server::local_cache::blob_path(&rig.cache, &blob);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, bytes).unwrap();
    }
    rig.core.prefetch_here(1);
    assert!(blob_fills(&rig).is_empty());
    assert_eq!(pinned(&rig).len(), 4);
}

/// Asserts that a prefetched blob is opened under passthrough with no fill, and registered.
#[test]
fn a_prefetched_blob_is_opened_by_passthrough_with_no_fill() {
    let rig = Rig::new("tfs-prefetch-passthrough", sample());
    rig.core.enable_passthrough();
    let _ = rig.core.attach_asking(&root(&rig), RUN, true).unwrap();
    until(|| rig.core.prefetching() == 0);
    let fills = blob_fills(&rig).len();
    let kernel = FakeKernel::new();
    assert!(
        kernel
            .open(&rig.core, crate::server::treefs::ino::of(1, 3))
            .is_ok()
    );
    assert_eq!(blob_fills(&rig).len(), fills);
    assert_eq!(kernel.registered.lock().unwrap().len(), 1);
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
    let pins = rig.upstream.asked("pin");
    assert_eq!(pins.len(), 1, "{pins:?}");
    assert_eq!(pinned(&rig).len(), 4);
}
