//! What the attaches and detaches of runs tell the node (`core/runs.rs`), on the rig's clock, which
//! only the test moves: an attach whose manifests are filled, split into its fetch, its flatten
//! and its append; an attach of a tree in memory already; an attach whose read fails, and those
//! refused before any read; a detach, and one of a run that attached nothing; and the split at
//! marks out of order.

use std::sync::Arc;
use std::time::Duration;

use super::super::Core;
use super::parts;
use crate::server::treefs::locked;
use crate::server::treefs::testing::{Rig, Upstream, sample, sha, until};

/// The run of the tests.
const RUN: &str = "run-1";

/// Lets every fill of a manifest go on when dropped, so that a test that fails ends.
struct Opens(Arc<Upstream>);

impl Drop for Opens {
    fn drop(&mut self) {
        self.0.permit_manifests(1 << 20);
    }
}

/// The reports of `verb` the node was sent, once `count` of them have come: they are sent by a
/// thread of their own.
fn reports(rig: &Rig, verb: &str, count: usize) -> Vec<String> {
    until(|| rig.upstream.asked(verb).len() >= count);
    rig.upstream.asked(verb)
}

/// Makes each write of the attachments file of `rig` take `ms` milliseconds of its clock.
fn saving_takes(rig: &Rig, ms: u64) {
    let clock = Arc::clone(&rig.clock);
    *locked(&rig.core.hooks.saving) = Some(Box::new(move |_: &Core| {
        clock.advance(Duration::from_millis(ms));
    }));
}

/// The root digest of the sample tree of `rig`.
fn root(rig: &Rig) -> String {
    rig.upstream.built.lock().unwrap().root.clone()
}

/// Asserts that an attach of the sample tree, whose 3 manifests the cache does not hold, is timed
/// from its start to its answer and split where its parts end: the fetch is the fills of its two
/// levels (5 ms and 20 ms), the flatten the 7 ms after the last fill to the end of the read, the
/// append the 3 ms of the write of the attachments file; `run-attached ok 35 25 7 3`.
#[test]
fn an_attach_is_split_into_its_fetch_its_flatten_and_its_append() {
    let rig = Rig::new("tfs-runs-split", sample());
    let root = root(&rig);
    rig.upstream.gate_manifests();
    let driver = {
        let upstream = Arc::clone(&rig.upstream);
        let clock = Arc::clone(&rig.clock);
        std::thread::spawn(move || {
            let opens = Opens(upstream);
            for (asked, level, ms) in [(1, 1, 5), (3, 2, 20)] {
                opens.0.wait_for_manifest_fills(asked);
                clock.advance(Duration::from_millis(ms));
                opens.0.permit_manifests(level);
            }
        })
    };
    let clock = Arc::clone(&rig.clock);
    *locked(&rig.core.hooks.flattened) = Some(Box::new(move |_: &Core| {
        clock.advance(Duration::from_millis(7));
    }));
    saving_takes(&rig, 3);
    let attached = rig.core.attach(&root, RUN);
    driver.join().unwrap();
    assert!(attached.is_ok(), "{attached:?}");
    assert_eq!(
        reports(&rig, "run-attached", 1),
        ["run-attached ok 35 25 7 3"]
    );
}

/// Asserts that an attach of a tree in memory already, for a second run, fetches nothing: its
/// read, of no time, is its fetch, and its flatten is 0; its append is the 2 ms of the write of
/// the attachments file.
#[test]
fn an_attach_of_a_tree_in_memory_has_no_flatten() {
    let rig = Rig::new("tfs-runs-memory", sample());
    let root = root(&rig);
    let _ = rig.core.attach(&root, RUN).unwrap();
    saving_takes(&rig, 2);
    let _ = rig.core.attach(&root, "run-2").unwrap();
    let told = reports(&rig, "run-attached", 2);
    assert_eq!(told[1], "run-attached ok 2 0 0 2");
}

/// Asserts that an attach whose read fails still reports its split, as `failed`: the root
/// manifest the upstream does not have takes 9 ms to be refused, all of it the fetch.
#[test]
fn an_attach_that_fails_reports_its_split_as_failed() {
    let rig = Rig::new("tfs-runs-failed", sample());
    rig.upstream.gate_manifests();
    let driver = {
        let upstream = Arc::clone(&rig.upstream);
        let clock = Arc::clone(&rig.clock);
        std::thread::spawn(move || {
            let opens = Opens(upstream);
            opens.0.wait_for_manifest_fills(1);
            clock.advance(Duration::from_millis(9));
            opens.0.permit_manifests(1);
        })
    };
    let attached = rig.core.attach(&sha(b"no such tree"), RUN);
    driver.join().unwrap();
    assert!(attached.is_err(), "{attached:?}");
    assert_eq!(
        reports(&rig, "run-attached", 1),
        ["run-attached failed 9 9 0 0"]
    );
}

/// Asserts that an attach refused before its read (a digest that is not one, a run name that is
/// not one) reports `failed` with no fetch and no flatten, even when time passes in it: none does
/// here, so every part is 0.
#[test]
fn an_attach_refused_before_its_read_reports_no_parts() {
    let rig = Rig::new("tfs-runs-refused", sample());
    let root = root(&rig);
    assert!(rig.core.attach("not-a-digest", RUN).is_err());
    assert!(rig.core.attach(&root, "not a run").is_err());
    assert_eq!(
        reports(&rig, "run-attached", 2),
        ["run-attached failed 0 0 0 0", "run-attached failed 0 0 0 0"]
    );
}

/// Asserts that a detach is timed from its start to its answer (the 4 ms of the write of the
/// attachments file) and reported `ok`, and that a detach of a run that attached nothing is
/// reported `failed`.
#[test]
fn a_detach_is_timed_and_one_of_a_run_with_nothing_is_failed() {
    let rig = Rig::new("tfs-runs-detach", sample());
    let root = root(&rig);
    let _ = rig.core.attach(&root, RUN).unwrap();
    saving_takes(&rig, 4);
    assert!(rig.core.detach(RUN).is_some());
    assert!(rig.core.detach("run-9").is_none());
    assert_eq!(
        reports(&rig, "run-detached", 2),
        ["run-detached ok 4", "run-detached failed 0"]
    );
}

/// Asserts the split of an attach from its marks: in order, the three parts between them; with no
/// fetch of its own, the whole read is the fetch; with no read, all of it is the append; and a
/// mark outside the attach is held to it, so that the parts always add up to the whole.
#[test]
fn the_parts_of_an_attach_add_up_to_it_whatever_its_marks() {
    assert_eq!(parts(10, 20, Some(18), Some(12)), [2, 6, 2]);
    assert_eq!(parts(10, 20, Some(18), None), [8, 0, 2]);
    assert_eq!(parts(10, 20, None, None), [0, 0, 10]);
    assert_eq!(parts(10, 20, Some(25), Some(5)), [0, 10, 0]);
    assert_eq!(parts(10, 20, Some(15), Some(17)), [5, 0, 5]);
}
