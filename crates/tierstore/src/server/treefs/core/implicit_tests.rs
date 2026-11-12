//! Trees that resolve on lookup: a lookup of a digest no run has attached reads and attaches the
//! tree with no run; the answers for a tree that cannot be read; the end of an implicit attachment
//! after `fs.tree_idle_secs` of no use, on the rig's clock, which only the test moves; the pins of
//! such a tree; a lookup and an idle detach while a successor takes over; and the thread that
//! detaches.

use std::sync::{Arc, mpsc};
use std::time::Duration;

use super::super::{Core, Errno, Looked};
use super::{look_every, sweep};
use crate::server::treefs::ino;
use crate::server::treefs::testing::{
    Built, Rig, TREE_IDLE, file, open_cached, sample, sha, until,
};

/// The inode number of the root of the tree numbered 1.
const ROOT: u64 = 1 << 24;

/// The root digest of the rig's tree.
fn root_of(rig: &Rig) -> String {
    rig.upstream.built.lock().unwrap().root.clone()
}

/// The inode number `trees/<digest>` resolves to.
fn resolved(core: &Core, digest: &str) -> u64 {
    match core.lookup(ino::TREES, digest).unwrap() {
        Looked::Found(entry) => entry.attr.ino,
        other => panic!("{digest}: {other:?}"),
    }
}

/// The inode number of `name` in the directory `parent`.
fn child(core: &Core, parent: u64, name: &str) -> u64 {
    match core.lookup(parent, name).unwrap() {
        Looked::Found(entry) => entry.attr.ino,
        other => panic!("{name}: {other:?}"),
    }
}

/// The manifest fills the upstream was asked for.
fn manifest_fills(rig: &Rig) -> Vec<String> {
    rig.upstream
        .asked("fill")
        .into_iter()
        .filter(|line| !line.contains(" from "))
        .collect()
}

/// Asserts that a lookup of a digest no run has attached reads the tree's manifests through the
/// fill socket, the root as `fill <digest>`, once each; answers the tree's root, numbered 1, with
/// no timeout; and attaches it with no run: listed as implicit, absent from the runs' listing and
/// the attachments file, and told to the cache by `pins-live` as an attached tree. A second lookup
/// reads nothing more.
#[test]
fn a_lookup_reads_the_tree_and_attaches_it_with_no_run() {
    let rig = Rig::new("tfs-imp-lookup", sample());
    let root = root_of(&rig);
    let Looked::Found(entry) = rig.core.lookup(ino::TREES, &root).unwrap() else {
        panic!("not found");
    };
    assert_eq!((entry.attr.ino, entry.ttl), (ROOT, Duration::ZERO));
    let fills = manifest_fills(&rig);
    assert_eq!(fills.len(), 3);
    assert_eq!(fills[0], format!("fill {root}"));
    assert_eq!(rig.core.implicit(), vec![1]);
    assert!(rig.core.attachments().is_empty());
    assert!(!rig.runtime.join("attachments").exists());
    rig.core.pins_live();
    assert_eq!(
        rig.upstream.asked("pins-live"),
        vec![format!("pins-live mount-1 {root}")]
    );
    assert_eq!(resolved(&rig.core, &root), ROOT);
    assert_eq!(manifest_fills(&rig).len(), 3);
    assert_eq!(child(&rig.core, ROOT, "big.bin"), ROOT + 1);
}

/// Asserts that a digest whose root manifest the upstream does not have is `ENOENT`, asked of the
/// upstream again at each lookup (a tree written later is found), and takes no tree number: the
/// tree that resolves next is numbered 1.
#[test]
fn a_digest_the_upstream_has_no_tree_of_is_not_there_and_takes_no_number() {
    let rig = Rig::new("tfs-imp-absent", sample());
    let other = sha(b"a tree nobody wrote");
    for asked in 1..=2 {
        assert_eq!(rig.core.lookup(ino::TREES, &other), Err(Errno::NoEnt));
        assert_eq!(manifest_fills(&rig), vec![format!("fill {other}"); asked]);
    }
    assert!(rig.core.implicit().is_empty());
    assert_eq!(
        rig.core
            .table
            .find(&crate::tree_manifest::TreeDigest::from_hex(&other).unwrap()),
        None
    );
    let later = Built::of(vec![("only", file(b"later"))]);
    let digest = later.root.clone();
    {
        let mut built = rig.upstream.built.lock().unwrap();
        built.manifests.extend(later.manifests);
        built.blobs.extend(later.blobs);
    }
    assert_eq!(resolved(&rig.core, &digest), ROOT);
}

/// Asserts that a tree that cannot be read for any reason but its root's absence is `EIO` and
/// takes no tree number: a manifest below the root that the upstream does not have, a root the
/// fill socket refuses (`mismatch`), and a table of tree numbers with every number taken.
#[test]
fn a_tree_that_cannot_be_read_or_numbered_is_an_io_error() {
    let rig = Rig::new("tfs-imp-broken", sample());
    let root = root_of(&rig);
    let kept = {
        let mut built = rig.upstream.built.lock().unwrap();
        let below: Vec<String> = built
            .manifests
            .keys()
            .filter(|digest| **digest != root)
            .cloned()
            .collect();
        below
            .iter()
            .map(|digest| (digest.clone(), built.manifests.remove(digest).unwrap()))
            .collect::<Vec<_>>()
    };
    assert_eq!(rig.core.lookup(ino::TREES, &root), Err(Errno::Io));
    assert!(rig.core.implicit().is_empty());
    rig.upstream.built.lock().unwrap().manifests.extend(kept);

    let refused = Rig::new("tfs-imp-refused", sample());
    let digest = root_of(&refused);
    let _ = refused
        .upstream
        .failing
        .lock()
        .unwrap()
        .insert(digest.clone(), "mismatch".to_owned());
    assert_eq!(refused.core.lookup(ino::TREES, &digest), Err(Errno::Io));
    assert!(refused.core.implicit().is_empty());

    rig.core.table.exhaust();
    assert_eq!(rig.core.lookup(ino::TREES, &root), Err(Errno::Io));
    assert!(rig.core.implicit().is_empty());
}

/// Asserts that many lookups at once of one tree no run has attached give one tree number and one
/// implicit attachment.
#[test]
fn lookups_of_one_tree_at_once_attach_it_once() {
    let rig = Rig::new("tfs-imp-race", sample());
    let root = root_of(&rig);
    std::thread::scope(|scope| {
        for _ in 0..8 {
            let _ = scope.spawn(|| assert_eq!(resolved(&rig.core, &root), ROOT));
        }
    });
    assert_eq!(rig.core.implicit(), vec![1]);
    assert_eq!(
        rig.core
            .table
            .find(&crate::tree_manifest::TreeDigest::from_hex(&root).unwrap()),
        Some(1)
    );
}

/// Asserts the idle time of an implicit attachment, on the rig's clock: a tree is not detached
/// before `fs.tree_idle_secs` with no use; a request on one of its inodes is a use that starts the
/// time again; at the full time the tree is detached and unpinned, and leaves memory once the
/// kernel forgets its inodes; and a lookup attaches it again, under the same number.
#[test]
fn an_implicit_attachment_ends_after_the_idle_time_with_no_use() {
    let rig = Rig::new("tfs-imp-idle", sample());
    let root = root_of(&rig);
    assert_eq!(resolved(&rig.core, &root), ROOT);
    let step = TREE_IDLE / 2;
    rig.clock.advance(step);
    assert_eq!(rig.core.detach_idle(), 0);
    assert!(rig.core.getattr(ROOT + 1).is_ok());
    rig.clock.advance(step);
    assert_eq!(rig.core.detach_idle(), 0);
    assert_eq!(rig.core.implicit(), vec![1]);
    assert!(rig.upstream.asked("unpin").is_empty());
    rig.clock.advance(step - Duration::from_millis(1));
    assert_eq!(rig.core.detach_idle(), 0);
    rig.clock.advance(Duration::from_millis(1));
    assert_eq!(rig.core.detach_idle(), 1);
    assert!(rig.core.implicit().is_empty());
    assert_eq!(
        rig.upstream.asked("unpin"),
        vec![format!("unpin mount-1 {root}")]
    );
    // The kernel holds the root: the tree stays in memory until it is forgotten.
    assert_eq!(rig.core.trees_in_memory(), 1);
    rig.core.forget(ROOT, 1);
    assert_eq!(rig.core.trees_in_memory(), 0);
    assert_eq!(rig.core.detach_idle(), 0);
    assert_eq!(resolved(&rig.core, &root), ROOT);
    assert_eq!(rig.core.implicit(), vec![1]);
}

/// Asserts that a tree a lookup attached again while it was still in memory (the kernel held its
/// inodes) is not read again: its manifests are gone from the cache and the upstream, and the
/// lookup still answers it.
#[test]
fn a_tree_in_memory_is_attached_again_without_a_read() {
    let rig = Rig::new("tfs-imp-again", sample());
    let root = root_of(&rig);
    assert_eq!(resolved(&rig.core, &root), ROOT);
    rig.clock.advance(TREE_IDLE);
    assert_eq!(rig.core.detach_idle(), 1);
    rig.upstream.built.lock().unwrap().manifests.clear();
    std::fs::remove_dir_all(&rig.cache).unwrap();
    std::fs::create_dir_all(&rig.cache).unwrap();
    let before = manifest_fills(&rig).len();
    assert_eq!(resolved(&rig.core, &root), ROOT);
    assert_eq!(manifest_fills(&rig).len(), before);
    assert_eq!(rig.core.implicit(), vec![1]);
}

/// Asserts that, for a frontend with no forget (NFS), a tree whose implicit attachment ends leaves
/// memory at once, and a request that names one of its inodes reads it again.
#[test]
fn an_uncounted_tree_leaves_memory_when_its_implicit_attachment_ends() {
    let rig = Rig::uncounted("tfs-imp-nfs", sample());
    let root = root_of(&rig);
    assert_eq!(resolved(&rig.core, &root), ROOT);
    rig.clock.advance(TREE_IDLE);
    assert_eq!(rig.core.detach_idle(), 1);
    assert_eq!(rig.core.trees_in_memory(), 0);
    assert!(rig.core.getattr(ROOT + 1).is_ok());
    assert_eq!(rig.core.trees_in_memory(), 1);
    assert!(rig.core.implicit().is_empty());
}

/// Asserts the pins of a tree a lookup attached: an open pins its blob for the tree; when the
/// implicit attachment ends the tree is unpinned, and an open of another file of it, through an
/// inode the kernel still holds, fills the blob and pins nothing (no attachment would take the pin
/// away).
#[test]
fn a_tree_is_pinned_only_while_attached() {
    let rig = Rig::new("tfs-imp-pins", sample());
    let root = root_of(&rig);
    assert_eq!(resolved(&rig.core, &root), ROOT);
    let big = child(&rig.core, ROOT, "big.bin");
    let bin = child(&rig.core, ROOT, "bin");
    let cc = child(&rig.core, bin, "cc");
    let _ = open_cached(&rig.core, big, false).unwrap();
    assert_eq!(rig.upstream.asked("pin").len(), 1);
    rig.clock.advance(TREE_IDLE);
    assert_eq!(rig.core.detach_idle(), 1);
    let blob_fills = rig.upstream.asked("fill").len();
    let _ = open_cached(&rig.core, cc, false).unwrap();
    assert_eq!(rig.upstream.asked("fill").len(), blob_fills + 1);
    assert_eq!(rig.upstream.asked("pin").len(), 1);
}

/// Asserts that a run's attachment and an implicit one of the same tree end apart: the idle end
/// of the implicit one leaves the tree attached and pinned for the run, a lookup while a run has
/// the tree makes no implicit attachment, and the detach of the run then unpins it.
#[test]
fn a_tree_a_run_also_has_stays_attached_when_its_implicit_attachment_ends() {
    let rig = Rig::new("tfs-imp-run", sample());
    let root = root_of(&rig);
    assert_eq!(resolved(&rig.core, &root), ROOT);
    let _ = rig.core.attach(&root, "run-1").unwrap();
    rig.clock.advance(TREE_IDLE);
    assert_eq!(rig.core.detach_idle(), 0);
    assert!(rig.core.implicit().is_empty());
    assert!(rig.upstream.asked("unpin").is_empty());
    assert_eq!(resolved(&rig.core, &root), ROOT);
    assert!(rig.core.implicit().is_empty());
    assert!(rig.core.detach("run-1").is_some());
    assert_eq!(
        rig.upstream.asked("unpin"),
        vec![format!("unpin mount-1 {root}")]
    );
}

/// Asserts that a run's detach leaves a tree a lookup attached: it stays attached, and is not
/// unpinned, until its own idle end.
#[test]
fn a_runs_detach_leaves_an_implicit_attachment() {
    let rig = Rig::new("tfs-imp-detach", sample());
    let root = root_of(&rig);
    let _ = rig.core.attach(&root, "run-1").unwrap();
    let other = rig
        .upstream
        .built
        .lock()
        .unwrap()
        .add_tree(vec![("x", file(b"x"))]);
    assert_eq!(resolved(&rig.core, &other), 2 << 24);
    let _ = rig.core.attach(&other, "run-1").unwrap();
    assert!(rig.core.detach("run-1").is_some());
    assert_eq!(
        rig.upstream.asked("unpin"),
        vec![format!("unpin mount-1 {root}")]
    );
    assert_eq!(rig.core.implicit(), vec![2]);
    assert_eq!(resolved(&rig.core, &other), 2 << 24);
}

/// Asserts what a lookup does while a successor takes over (`hold`): a tree that has a tree number
/// is answered and not attached, one that has none is `EIO` and gets none, and no idle tree is
/// detached; when the hold ends, the idle tree is.
#[test]
fn while_held_a_lookup_numbers_and_attaches_nothing_and_nothing_is_detached() {
    let rig = Rig::new("tfs-imp-held", sample());
    let root = root_of(&rig);
    assert_eq!(resolved(&rig.core, &root), ROOT);
    let other = rig
        .upstream
        .built
        .lock()
        .unwrap()
        .add_tree(vec![("x", file(b"x"))]);
    let _ = rig.core.attach(&other, "run-1").unwrap();
    assert!(rig.core.detach("run-1").is_some());
    rig.clock.advance(TREE_IDLE);
    rig.core.hold(true);
    assert_eq!(rig.core.detach_idle(), 0);
    assert_eq!(rig.core.implicit(), vec![1]);
    assert_eq!(resolved(&rig.core, &other), 2 << 24);
    assert_eq!(rig.core.implicit(), vec![1]);
    let third = rig
        .upstream
        .built
        .lock()
        .unwrap()
        .add_tree(vec![("y", file(b"y"))]);
    assert_eq!(rig.core.lookup(ino::TREES, &third), Err(Errno::Io));
    assert_eq!(
        rig.core
            .table
            .find(&crate::tree_manifest::TreeDigest::from_hex(&third).unwrap()),
        None
    );
    rig.core.hold(false);
    assert_eq!(rig.core.detach_idle(), 1);
    assert!(rig.core.implicit().is_empty());
}

/// Asserts that the thread started by `sweep_idle` detaches a tree once the clock is past its idle
/// time, without a call from the test.
#[test]
fn the_idle_thread_detaches_an_idle_tree() {
    let rig = Rig::new("tfs-imp-thread", sample());
    let root = root_of(&rig);
    assert_eq!(resolved(&rig.core, &root), ROOT);
    rig.core.sweep_idle(Duration::from_millis(1)).unwrap();
    rig.clock.advance(TREE_IDLE);
    until(|| !rig.upstream.asked("unpin").is_empty());
    assert!(rig.core.implicit().is_empty());
    assert_eq!(
        rig.upstream.asked("unpin"),
        vec![format!("unpin mount-1 {root}")]
    );
}

/// Asserts that the loop of the idle thread detaches the idle trees of the core between its waits,
/// and ends when the other end of its channel sends, or is dropped.
#[test]
fn the_idle_loop_ends_when_its_channel_ends() {
    let rig = Rig::new("tfs-imp-loop", sample());
    let root = root_of(&rig);
    assert_eq!(resolved(&rig.core, &root), ROOT);
    rig.clock.advance(TREE_IDLE);
    let (stop, wait) = mpsc::channel::<()>();
    let core = Arc::downgrade(&rig.core);
    let weak = core.clone();
    let looping = std::thread::spawn(move || sweep(&wait, &weak, Duration::from_millis(1)));
    until(|| rig.core.implicit().is_empty());
    stop.send(()).unwrap();
    looping.join().unwrap();
    let (stop, wait) = mpsc::channel::<()>();
    drop(stop);
    sweep(&wait, &core, Duration::from_secs(3600));
}

/// Asserts how often the idle trees are looked for: a quarter of the idle time, at least a second
/// and at most a minute apart.
#[test]
fn idle_trees_are_looked_for_every_quarter_of_the_idle_time_within_bounds() {
    assert_eq!(
        look_every(Duration::from_secs(900)),
        Duration::from_secs(60)
    );
    assert_eq!(look_every(Duration::from_secs(40)), Duration::from_secs(10));
    assert_eq!(look_every(Duration::from_secs(1)), Duration::from_secs(1));
}
