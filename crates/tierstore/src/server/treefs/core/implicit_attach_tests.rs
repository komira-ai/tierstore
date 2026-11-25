//! The read of a tree that a lookup attaches, as the fill socket sees it: each level of the tree's
//! manifests asked for at once, so the attach waits for a fill for each level and not one for each
//! directory; and what the lookup tells the node and says on stderr of each attach, on the rig's
//! clock, which only the test moves: an attach whose manifests were filled, one whose manifests the
//! cache held, a digest the upstream has no tree of, and a tree with a manifest that is not one.

use std::sync::Arc;
use std::time::Duration;

use super::super::{Errno, Looked};
use crate::server::treefs::ino;
use crate::server::treefs::testing::{Built, Logged, Rig, Upstream, dir, file, logged, sha, until};

/// The inode number of the root of the tree numbered 1.
const ROOT: u64 = 1 << 24;

/// The fills of manifests the upstream was asked for, in all.
fn manifest_fills(upstream: &Upstream) -> usize {
    let fills = upstream.asked("fill");
    fills.iter().filter(|line| !line.contains(" from ")).count()
}

/// A tree of 13 distinct manifests on 3 levels: the root holds the directories `d0` to `d3`, and
/// each of them the directories `s0` and `s1`, each holding a file of its own.
fn wide() -> Built {
    let contents: Vec<Vec<u8>> = (0..8).map(|n| format!("file {n}").into_bytes()).collect();
    let level = |i: usize| {
        let below = |j: usize| dir(vec![("f", file(&contents[2 * i + j]))]);
        dir(vec![("s0", below(0)), ("s1", below(1))])
    };
    Built::of(vec![
        ("d0", level(0)),
        ("d1", level(1)),
        ("d2", level(2)),
        ("d3", level(3)),
    ])
}

/// Lets every fill of a manifest go on when dropped: a test whose fills never come together ends
/// with the lookup answered, not waiting at the gate.
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

/// The lines a lookup said of its attaches.
fn said(log: &Logged) -> Vec<String> {
    let text = log.text();
    let ours = |line: &&str| line.starts_with("tierstore fs: lookup of ");
    text.lines().filter(ours).map(str::to_owned).collect()
}

/// Asserts that a lookup of a tree of 3 levels whose manifests the cache does not hold asks the
/// fill socket for each level at once (the root; then the 4 below it, all asked before any is
/// answered; then the 8 below those), so the lookup waits for 3 fills and not 13, with 8 in flight
/// at most; and that it reports the attach (`attached ok 65`: 5, 20 and 40 ms for the three levels
/// on the rig's clock) and the 13 manifests, all filled, with the fill of each in its bucket
/// (`fetched`), and its 3 levels and 4 rounds (the 3 levels of fills, and the `fill-tree` the
/// rig's node refused as unknown: `levels`), and says them on stderr by level.
#[test]
fn a_lookup_fetches_a_level_of_manifests_at_once_and_reports_the_attach() {
    let rig = Rig::new("tfs-imp-levels", wide());
    let root = rig.upstream.built.lock().unwrap().root.clone();
    rig.upstream.gate_manifests();
    let driver = {
        let upstream = Arc::clone(&rig.upstream);
        let clock = Arc::clone(&rig.clock);
        std::thread::spawn(move || {
            let opens = Opens(upstream);
            for (asked, level, ms) in [(1, 1, 5), (5, 4, 20), (13, 8, 40)] {
                // A level is asked for whole before any of it is answered: every fill of it has
                // entered the gate, and is counted in flight, before any is let go on.
                opens.0.wait_for_manifest_fills(asked);
                clock.advance(Duration::from_millis(ms));
                opens.0.permit_manifests(level);
            }
        })
    };
    let log = Logged::default();
    let found = {
        let _logging = logged(&log, tracing::Level::INFO);
        rig.core.lookup(ino::TREES, &root).unwrap()
    };
    driver.join().unwrap();
    let Looked::Found(entry) = &found else {
        panic!("not found: {found:?}");
    };
    assert_eq!(entry.attr.ino, ROOT);
    assert_eq!(manifest_fills(&rig.upstream), 13);
    assert_eq!(rig.upstream.most_manifest_fills(), 8);
    let d3 = rig.core.lookup(ROOT, "d3").unwrap();
    assert!(matches!(d3, Looked::Found(_)), "{d3:?}");
    assert_eq!(reports(&rig, "attached", 1), ["attached ok 65"]);
    assert_eq!(
        reports(&rig, "fetched", 1),
        ["fetched 0 13 0 405 0 0 1 0 4 8 0 0 0 0 0 0 0 0 0 0 0"]
    );
    assert_eq!(reports(&rig, "levels", 1), ["levels 3 4"]);
    let line = format!(
        "tierstore fs: lookup of {root}: ok in 65 ms; 13 manifests: 0 from the cache, \
         13 filled, 0 not had; 3 levels; fills by level: 1 4 8, slowest ms by level: 5 20 40; \
         fill-tree unknown to the node in 0 ms"
    );
    assert_eq!(said(&log), [line]);
}

/// Asserts that a lookup of a tree whose manifests the cache holds reads them all from the cache
/// and asks the fill socket for none, not even a `fill-tree`, and reports so: a second core over
/// the cache the first filled.
#[test]
fn a_lookup_of_a_tree_the_cache_holds_reports_its_manifests_from_the_cache() {
    let rig = Rig::new("tfs-imp-cached", wide());
    let root = rig.upstream.built.lock().unwrap().root.clone();
    let first = rig.core.lookup(ino::TREES, &root);
    assert!(matches!(first, Ok(Looked::Found(_))), "{first:?}");
    let _ = reports(&rig, "attached", 1);
    let second = rig.second_core();
    let log = Logged::default();
    let found = {
        let _logging = logged(&log, tracing::Level::INFO);
        second.lookup(ino::TREES, &root)
    };
    assert!(matches!(found, Ok(Looked::Found(_))), "{found:?}");
    assert_eq!(manifest_fills(&rig.upstream), 13);
    assert_eq!(reports(&rig, "attached", 2)[1], "attached ok 0");
    assert_eq!(
        reports(&rig, "fetched", 2)[1],
        "fetched 13 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0"
    );
    assert_eq!(reports(&rig, "levels", 2)[1], "levels 3 0");
    let line = format!(
        "tierstore fs: lookup of {root}: ok in 0 ms; 13 manifests: 13 from the cache, \
         0 filled, 0 not had; 3 levels; fills by level: 0 0 0, slowest ms by level: 0 0 0"
    );
    assert_eq!(said(&log), [line]);
}

/// Asserts that a digest the upstream has no tree of is `ENOENT` and reported `absent`, its root
/// fill counted as not had; and that a tree with a directory whose manifest is not a manifest is
/// `EIO` and reported `failed`, as before, both of its manifests filled and had: the bytes were
/// there, and are refused when they are read.
#[test]
fn an_absent_and_a_malformed_tree_are_reported_by_their_outcome() {
    let rig = Rig::new("tfs-imp-refused", wide());
    let absent = sha(b"a tree nobody wrote");
    assert_eq!(rig.core.lookup(ino::TREES, &absent), Err(Errno::NoEnt));
    assert_eq!(reports(&rig, "attached", 1), ["attached absent 0"]);
    assert_eq!(
        reports(&rig, "fetched", 1),
        ["fetched 0 0 1 0 1 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0"]
    );

    let not_a_manifest = b"not a manifest".to_vec();
    let below = sha(&not_a_manifest);
    let text = format!(
        r#"{{"v":1,"children":[{{"name":"d","kind":"dir","sha256":"{below}","size":14}}]}}"#
    );
    let malformed = sha(text.as_bytes());
    {
        let mut built = rig.upstream.built.lock().unwrap();
        built.manifests.insert(below, not_a_manifest);
        built.manifests.insert(malformed.clone(), text.into_bytes());
    }
    assert_eq!(rig.core.lookup(ino::TREES, &malformed), Err(Errno::Io));
    assert_eq!(reports(&rig, "attached", 2)[1], "attached failed 0");
    assert_eq!(
        reports(&rig, "fetched", 2)[1],
        "fetched 0 2 0 0 2 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0"
    );
    assert!(rig.core.implicit().is_empty());
}

/// Asserts that a fill the socket answers `ok` but that leaves no manifest in the cache counts
/// that manifest as not had, its fill timed all the same, and the lookup is `EIO`, reported
/// `failed`.
#[test]
fn a_fill_that_leaves_no_manifest_counts_it_as_not_had() {
    let built = Built::of(vec![("only", dir(vec![("f", file(b"x"))]))]);
    let rig = Rig::new("tfs-imp-unhad", built);
    let (root, below) = {
        let built = rig.upstream.built.lock().unwrap();
        let mut keys = built.manifests.keys();
        let below = keys.find(|sha| **sha != built.root).cloned();
        (built.root.clone(), below.unwrap())
    };
    rig.upstream
        .failing
        .lock()
        .unwrap()
        .insert(below, "ok".to_owned());
    assert_eq!(rig.core.lookup(ino::TREES, &root), Err(Errno::Io));
    assert_eq!(reports(&rig, "attached", 1), ["attached failed 0"]);
    assert_eq!(
        reports(&rig, "fetched", 1),
        ["fetched 0 1 1 0 2 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0"]
    );
}
