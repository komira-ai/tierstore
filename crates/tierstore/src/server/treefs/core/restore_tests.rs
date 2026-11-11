//! The restore of the attachments against a fake fill socket whose fills of manifests the test
//! paces by its own events.

use crate::server::treefs::testing::{Built, Rig, file, sample};
use crate::tree_manifest::TreeDigest;

fn digest(hex: &str) -> TreeDigest {
    TreeDigest::from_hex(hex).unwrap()
}

/// A rig over the sample tree and `count` more trees of one manifest each; their roots.
fn many(name: &str, count: usize) -> (Rig, Vec<String>) {
    let rig = Rig::new(name, Built::of(Vec::new()));
    let roots = (0..count)
        .map(|index| {
            let content = format!("tree {index}");
            rig.upstream
                .built
                .lock()
                .unwrap()
                .add_tree(vec![("f", file(content.as_bytes()))])
        })
        .collect();
    (rig, roots)
}

fn saved(roots: &[String]) -> Vec<(TreeDigest, String)> {
    roots
        .iter()
        .enumerate()
        .map(|(index, root)| (digest(root), format!("run-{index}")))
        .collect()
}

/// Asserts that a restore reads the manifests of several trees at once, up to
/// `fs.reattach_concurrency` and no more: the fills are held, three are in flight and a fourth is
/// received only when one of them is answered; every tree is attached when the restore returns.
#[test]
fn a_restore_reads_trees_together_up_to_the_limit() {
    let (rig, roots) = many("tfs-restore-limit", 6);
    rig.upstream.gate_manifests();
    std::thread::scope(|scope| {
        let core = &rig.core;
        let restoring = scope.spawn(|| core.restore(saved(&roots), 3));
        rig.upstream.wait_for_manifest_fills(3);
        for received in 4..=6 {
            rig.upstream.permit_manifests(1);
            rig.upstream.wait_for_manifest_fills(received);
        }
        rig.upstream.permit_manifests(6);
        restoring.join().unwrap();
    });
    assert_eq!(rig.upstream.most_manifest_fills(), 3);
    assert_eq!(rig.core.attachments().len(), 6);
}

/// Asserts that a limit of one reads the trees one after the other: a fill is received only when
/// the one before it is answered.
#[test]
fn a_limit_of_one_reads_the_trees_in_turn() {
    let (rig, roots) = many("tfs-restore-serial", 3);
    rig.upstream.gate_manifests();
    std::thread::scope(|scope| {
        let core = &rig.core;
        let restoring = scope.spawn(|| core.restore(saved(&roots), 1));
        for received in 1..=3 {
            rig.upstream.wait_for_manifest_fills(received);
            rig.upstream.permit_manifests(1);
        }
        restoring.join().unwrap();
    });
    assert_eq!(rig.upstream.most_manifest_fills(), 1);
    assert_eq!(rig.core.attachments().len(), 3);
}

/// Asserts that a tree that cannot be read does not lose the others: the trees around it are
/// attached, it is kept in the file after them, and the file lists the others in the order it had, whichever
/// read finished first.
#[test]
fn a_tree_that_fails_does_not_lose_the_others() {
    let (rig, mut roots) = many("tfs-restore-error", 5);
    let gone = "ab".repeat(32);
    roots.insert(2, gone.clone());
    rig.core.restore(saved(&roots), 4);
    assert_eq!(rig.core.attachments().len(), 5);
    let want: String = roots
        .iter()
        .enumerate()
        .filter(|(_, root)| **root != gone)
        .chain(roots.iter().enumerate().filter(|(_, root)| **root == gone))
        .map(|(index, root)| format!("{root} run-{index}\n"))
        .collect();
    let text = std::fs::read_to_string(rig.runtime.join("attachments")).unwrap();
    assert_eq!(text, want);
    assert!(text.contains(&format!("{gone} run-2\n")));
    assert_eq!(rig.core.detach("run-2"), Some(Vec::new()));
    let text = std::fs::read_to_string(rig.runtime.join("attachments")).unwrap();
    assert!(!text.contains(&gone));
}

/// Asserts that a tree several runs attach is read once, and each run is attached to it.
#[test]
fn a_tree_of_several_runs_is_read_once() {
    let rig = Rig::new("tfs-restore-shared", sample());
    let root = rig.upstream.built.lock().unwrap().root.clone();
    let runs = (0..5).map(|index| (digest(&root), format!("run-{index}")));
    rig.core.restore(runs.collect(), 4);
    assert_eq!(rig.upstream.asked("fill").len(), 3);
    assert_eq!(rig.core.attachments().len(), 5);
    assert!(rig.core.detach("run-0").is_some());
}

/// Asserts that a restore of nothing, and a limit of zero, read nothing and do not stop.
#[test]
fn a_restore_of_nothing_reads_nothing() {
    let rig = Rig::new("tfs-restore-none", sample());
    rig.core.restore(Vec::new(), 0);
    assert!(rig.core.attachments().is_empty());
    assert!(rig.upstream.asked("fill").is_empty());
}
