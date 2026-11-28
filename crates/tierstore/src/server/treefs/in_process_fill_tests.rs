//! The words the cache answers the core's fills, pins and reports with, asked in process: what a
//! fill puts in the cache and says when it fails, the pins, and the counts a report adds to the
//! cache's metrics.

use std::sync::Arc;

use super::{LONG, MOUNT, after, ask, blob_of, fill_blob_of, in_process, shared};
use crate::Op;
use crate::server::local_cache::attaches::{Attach, Levels, Outcome};
use crate::server::local_cache::descriptors::Descriptors;
use crate::server::local_cache::pack_testing::Pack;
use crate::server::local_cache::testing::{Rig, digest, pattern};
use crate::server::local_cache::{PinError, blob_path};
use crate::server::treefs::fills::Report;
use crate::server::treefs::in_process::InProcess;

/// Asserts that a fill asked in process puts the blob in the cache: the answer is `Ok`, the file
/// holds the blob at the path the layout gives its key, and the fill is counted in the cache's
/// metrics.
#[tokio::test]
async fn a_fill_puts_the_blob_in_the_cache() {
    let rig = Rig::new("tfs-in-fill");
    let pack = Pack::put(&rig, "packs/one", &[2000, 3000]).await;
    let over = shared(&rig.cache, None);
    assert_eq!(fill_blob_of(&over, blob_of(&pack, 1)).await, Ok(()));
    let file = blob_path(&rig.config.path, &pack.blobs[1].0);
    assert_eq!(std::fs::read(file).unwrap(), pack.content(1));
    let metrics = rig.cache.metrics();
    assert!(
        metrics.contains("tierstore_cache_fills_total 1\n"),
        "{metrics}"
    );
}

/// Asserts that a fill whose upstream refuses this node's key (an answer 403, `Error::Denied`) is
/// answered with exactly the words `upstream 403`, for a blob and for a manifest, and that the
/// requests go on being answered. Catches a refusal answered `upstream 500`.
#[tokio::test]
async fn a_fill_the_upstream_refuses_is_answered_upstream_403() {
    let rig = Rig::new("tfs-in-refused");
    let pack = Pack::put(&rig, "packs/refused", &[500]).await;
    let over = shared(&rig.cache, None);
    let refused = || crate::Error::Denied("store answered 403 AccessDenied: no".to_owned());
    rig.upstream.fail_next_with(Op::Get, refused());
    assert_eq!(
        fill_blob_of(&over, blob_of(&pack, 0)).await,
        Err("upstream 403".to_owned())
    );
    rig.upstream.fail_next_with(Op::Head, refused());
    let manifest = digest(b"a manifest");
    let answered = ask(&over, move |over| {
        over.fill_manifest(&manifest, after(u64::MAX))
    })
    .await;
    assert_eq!(answered, Err("upstream 403".to_owned()));
    assert_eq!(fill_blob_of(&over, blob_of(&pack, 0)).await, Ok(()));
}

/// Asserts the words of the fills that fail, one after the other on one core: a mismatch, a
/// missing source, a blob too large for the budget; and then the fills that work, of a blob and
/// of a manifest the upstream holds.
#[tokio::test]
async fn the_words_of_a_failed_fill_are_the_caches_and_the_fills_after_it_work() {
    let rig = Rig::new("tfs-in-words");
    let pack = Pack::put(&rig, "packs/two", &[500, 600]).await;
    let over = shared(&rig.cache, None);
    let ((sha, offset, size), key) = blob_of(&pack, 0);
    let words = ask(&over, move |over| {
        [
            over.fill_blob((sha.as_str(), size), (key.as_str(), offset + 1), None),
            over.fill_blob((sha.as_str(), size), ("packs/none", 0), None),
            over.fill_blob((sha.as_str(), 2 << 20), (key.as_str(), 0), None),
        ]
    })
    .await;
    let failed = |words: &str| Err::<(), String>(words.to_owned());
    assert_eq!(
        words,
        [
            failed("mismatch"),
            failed("upstream 404"),
            failed("no space")
        ]
    );
    assert_eq!(fill_blob_of(&over, blob_of(&pack, 0)).await, Ok(()));
    let body = pattern(800);
    let named = digest(&body);
    rig.put(&format!("cas/l/{named}"), &body).await;
    let manifest = ask(&over, move |over| {
        over.fill_manifest(&named, after(u64::MAX))
    })
    .await;
    assert_eq!(manifest, Ok(()));
}

/// Asserts that a `Started` report ends the span of the run the path is a source of, which the
/// cache stamped when it was requested: 70 ms on the cache's clock; a second `Started` of the run
/// adds nothing.
#[tokio::test]
async fn a_started_report_ends_the_span_of_its_run() {
    let rig = Rig::new("tfs-in-started");
    rig.cache.run_requested("mount-provider/61/6a73");
    rig.clock.advance(70);
    let over = in_process(&rig.cache, None, LONG);
    over.report(Report::Started("mount-provider/61/6a73/0".to_owned()));
    over.report(Report::Started("mount-provider/61/6a73/1".to_owned()));
    let metrics = rig.cache.metrics();
    assert!(
        metrics.contains("tierstore_request_to_start_seconds_sum 0.07\n"),
        "{metrics}"
    );
    assert!(
        metrics.contains("tierstore_request_to_start_seconds_count 1\n"),
        "{metrics}"
    );
}

/// Pins `blobs` for `tree` of the mount, made in process.
async fn pin(
    over: &Arc<InProcess>,
    tree: &'static str,
    blobs: Vec<String>,
) -> Result<String, String> {
    let answered = ask(over, move |over| {
        let blobs: Vec<&str> = blobs.iter().map(String::as_str).collect();
        over.pin(MOUNT, tree, &blobs)
    })
    .await;
    answered.map_err(|why| why.to_string())
}

/// Asserts that the pin requests reach the cache's pins and are answered with the numbers: a pin
/// of two held blobs and one the cache does not hold, the same pin again (the same answer), an
/// unpin of the tree (the blobs it had pinned), and a pins-live that keeps one of two trees.
#[tokio::test]
async fn the_pin_requests_pin_unpin_and_keep_the_live_trees() {
    let rig = Rig::new("tfs-in-pin");
    let pack = Pack::put(&rig, "packs/pins", &[300, 300]).await;
    let over = shared(&rig.cache, None);
    for i in 0..2 {
        assert_eq!(fill_blob_of(&over, blob_of(&pack, i)).await, Ok(()));
    }
    let (first, second) = (pack.blobs[0].0.clone(), pack.blobs[1].0.clone());
    let missing = digest(b"a blob nobody filled");
    let all = vec![first.clone(), second, missing];
    assert_eq!(
        pin(&over, "tree-a", all.clone()).await,
        Ok("ok 2 0 1".to_owned())
    );
    assert_eq!(pin(&over, "tree-a", all).await, Ok("ok 2 0 1".to_owned()));
    assert_eq!(rig.cache.pinned_bytes(), 600);
    assert_eq!(
        pin(&over, "tree-b", vec![first]).await,
        Ok("ok 1 0 0".to_owned())
    );
    let unpinned = ask(&over, |over| over.unpin(MOUNT, "tree-a")).await;
    assert_eq!(unpinned, Ok("ok 2".to_owned()));
    assert_eq!(rig.cache.pinned_bytes(), 300);
    let kept = ask(&over, |over| over.pins_live(MOUNT, &["tree-c".to_owned()])).await;
    assert_eq!(kept, Ok("ok 1".to_owned()));
    assert_eq!(rig.cache.pinned_bytes(), 0);
    let none = ask(&over, |over| over.pins_live(MOUNT, &[])).await;
    assert_eq!(none, Ok("ok 0".to_owned()));
}

/// Asserts the words of a pin request that the cache refuses: `read-only` for a cache that does
/// not hold the lock of its directory, and that the mount id `hold` is reserved.
#[test]
fn a_pin_request_of_a_read_only_cache_is_answered_read_only() {
    use super::super::pinned;
    assert_eq!(pinned(Err(PinError::ReadOnly)), "read-only");
    assert_eq!(pinned(Ok("1 0 0".to_owned())), "ok 1 0 0");
    assert_eq!(
        pinned(Err(PinError::ReservedMount("hold".to_owned()))),
        "the mount id \"hold\" is reserved for holds"
    );
}

/// Asserts that an `Attached`, a `Descriptors` and a `Levels` report made in process show in the
/// cache's families: the attach in the histogram of its outcome, the descriptors gauge at the last
/// report and the failures counter at the process's total, and the levels and rounds counters.
#[tokio::test]
async fn the_attached_descriptors_and_levels_reports_are_counted_in_the_cache() {
    let rig = Rig::new("tfs-in-counted");
    let over = in_process(&rig.cache, None, LONG);
    over.report(Report::Attached(Attach {
        outcome: Outcome::Ok,
        ms: 245,
    }));
    for (held, short) in [(40, 2), (35, 5)] {
        over.report(Report::Descriptors(Descriptors {
            mount: "boot-1".to_owned(),
            process: 7,
            held,
            short,
        }));
    }
    over.report(Report::Levels(Levels {
        levels: 9,
        rounds: 1,
    }));
    let metrics = rig.cache.metrics();
    for line in [
        "tierstore_fs_tree_attach_seconds_sum{outcome=\"ok\"} 0.245\n",
        "tierstore_fs_tree_attach_seconds_count{outcome=\"ok\"} 1\n",
        "tierstore_fs_cache_descriptors{mount=\"boot-1\"} 35\n",
        "tierstore_fs_open_descriptor_failures_total{mount=\"boot-1\"} 5\n",
        "tierstore_fs_tree_levels_total 9\n",
        "tierstore_fs_tree_fill_rounds_total 1\n",
    ] {
        assert!(metrics.contains(line), "{line}{metrics}");
    }
}
