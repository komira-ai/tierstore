//! The read of a tree that a lookup attaches, as the upstream sees it: the cache fills each level
//! of the tree's manifests at once (`fill-tree`), so the attach waits for a round of fills for each
//! level and not one for each directory; and what the lookup tells the cache (its metrics) and says
//! on stderr of each attach, on the rig's clock, which only the test moves: an attach whose
//! manifests the cache filled, one whose manifests the cache held, a digest the upstream has no
//! tree of, and a tree with a manifest that is not one.

use std::sync::Arc;
use std::time::Duration;

use super::super::{Errno, Looked};
use crate::server::treefs::ino;
use crate::server::treefs::testing::{Built, Logged, Rig, Upstream, dir, file, logged, sha};

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

/// The value of the series `name` (with its labels) in the metrics of the node's cache, as
/// rendered: what the core reported reaches them at once, on the thread that reports it.
fn series(rig: &Rig, name: &str) -> String {
    let text = rig.node.metrics();
    let prefix = format!("{name} ");
    let found = text.lines().find_map(|line| line.strip_prefix(&prefix));
    found
        .unwrap_or_else(|| panic!("no series {name} in {text}"))
        .to_owned()
}

/// The value of the counter `name` in the node's metrics.
fn counted(rig: &Rig, name: &str) -> u64 {
    series(rig, name).parse().unwrap()
}

/// What the node counted of the reports of the attaches of trees, in all.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Reported {
    /// The attaches that ended `ok`, `absent` and `failed`.
    attaches: [u64; 3],
    /// The manifests read from the cache directory, filled by the cache and not had.
    manifests: [u64; 3],
    /// The fills of manifests the core timed, in all.
    timed: u64,
    /// The levels the reads went down, and their rounds of fills.
    levels: [u64; 2],
}

/// What `rig`'s cache has been told so far.
fn reported(rig: &Rig) -> Reported {
    let attaches = ["ok", "absent", "failed"]
        .map(|outcome| format!("tierstore_fs_tree_attach_seconds_count{{outcome=\"{outcome}\"}}"));
    let manifests = ["cache", "fill", "error"]
        .map(|result| format!("tierstore_fs_manifest_fetch_total{{result=\"{result}\"}}"));
    Reported {
        attaches: attaches.map(|name| counted(rig, &name)),
        manifests: manifests.map(|name| counted(rig, &name)),
        timed: counted(rig, "tierstore_fs_manifest_fill_seconds_count"),
        levels: [
            counted(rig, "tierstore_fs_tree_levels_total"),
            counted(rig, "tierstore_fs_tree_fill_rounds_total"),
        ],
    }
}

/// The lines a lookup said of its attaches.
fn said(log: &Logged) -> Vec<String> {
    let text = log.text();
    let ours = |line: &&str| line.starts_with("tierstore fs: lookup of ");
    text.lines().filter(ours).map(str::to_owned).collect()
}

/// Asserts that a lookup of a tree of 3 levels whose manifests the cache does not hold has the
/// cache fill each level at once (the root; then the 4 below it, all asked before any is answered;
/// then the 8 below those), so the lookup waits for 3 rounds of fills and not 13, with 8 in flight
/// at most; that it reports the attach (`ok`, 65 ms: 5, 20 and 40 ms for the three levels on the
/// rig's clock) and the 13 manifests, all read from the cache after the fill (none filled by the
/// read itself, so no fill of the read is timed), its 3 levels and 1 round (the cache's fill of the
/// tree), and that the cache counted its 13 manifests as filled for a tree; and says them on
/// stderr by level.
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
    assert_eq!(
        reported(&rig),
        Reported {
            attaches: [1, 0, 0],
            manifests: [13, 0, 0],
            timed: 0,
            levels: [3, 1],
        }
    );
    assert_eq!(
        series(&rig, "tierstore_fs_tree_attach_seconds_sum{outcome=\"ok\"}"),
        "0.065"
    );
    assert_eq!(
        counted(
            &rig,
            "tierstore_cache_manifest_fill_seconds_count{via=\"tree\"}"
        ),
        13
    );
    assert_eq!(
        counted(
            &rig,
            "tierstore_cache_manifest_fill_seconds_count{via=\"fill\"}"
        ),
        0
    );
    let line = format!(
        "tierstore fs: lookup of {root}: ok in 65 ms; 13 manifests: 13 from the cache, \
         0 filled, 0 not had; 3 levels; fills by level: 0 0 0, slowest ms by level: 0 0 0; \
         fill-tree ok in 65 ms, 0 ms at the node, fills by level: 1 4 8"
    );
    assert_eq!(said(&log), [line]);
}

/// Asserts that a lookup of a tree whose manifests the cache holds reads them all from the cache
/// and has none filled, not even a `fill-tree`, and reports so: a second core over the cache the
/// first filled.
#[test]
fn a_lookup_of_a_tree_the_cache_holds_reports_its_manifests_from_the_cache() {
    let rig = Rig::new("tfs-imp-cached", wide());
    let root = rig.upstream.built.lock().unwrap().root.clone();
    let first = rig.core.lookup(ino::TREES, &root);
    assert!(matches!(first, Ok(Looked::Found(_))), "{first:?}");
    let before = reported(&rig);
    let second = rig.second_core();
    let log = Logged::default();
    let found = {
        let _logging = logged(&log, tracing::Level::INFO);
        second.lookup(ino::TREES, &root)
    };
    assert!(matches!(found, Ok(Looked::Found(_))), "{found:?}");
    assert_eq!(manifest_fills(&rig.upstream), 13);
    // The second lookup: one more attach `ok` in 0 ms, 13 manifests from the cache, no fill timed,
    // 3 more levels and no round.
    let after = reported(&rig);
    assert_eq!(
        after,
        Reported {
            attaches: [before.attaches[0] + 1, 0, 0],
            manifests: [before.manifests[0] + 13, 0, 0],
            timed: before.timed,
            levels: [before.levels[0] + 3, before.levels[1]],
        }
    );
    assert_eq!(
        series(&rig, "tierstore_fs_tree_attach_seconds_sum{outcome=\"ok\"}"),
        "0"
    );
    let line = format!(
        "tierstore fs: lookup of {root}: ok in 0 ms; 13 manifests: 13 from the cache, \
         0 filled, 0 not had; 3 levels; fills by level: 0 0 0, slowest ms by level: 0 0 0"
    );
    assert_eq!(said(&log), [line]);
}

/// Asserts that a digest the upstream has no tree of is `ENOENT` and reported `absent`, its root
/// fill counted as not had; and that a tree with a directory whose manifest is not a manifest is
/// `EIO` and reported `failed`, as before: the cache's fill of the tree kept both of its
/// manifests (their bytes are their digests'), so the read took both from the cache, and refused
/// the second when it read it.
#[test]
fn an_absent_and_a_malformed_tree_are_reported_by_their_outcome() {
    let rig = Rig::new("tfs-imp-refused", wide());
    let absent = sha(b"a tree nobody wrote");
    assert_eq!(rig.core.lookup(ino::TREES, &absent), Err(Errno::NoEnt));
    let first = reported(&rig);
    assert_eq!(first.attaches, [0, 1, 0]);
    // The root, not had: its fill failed, and the one fill the core timed took 0 ms.
    assert_eq!(first.manifests, [0, 0, 1]);
    assert_eq!(first.timed, 1);
    assert_eq!(
        counted(
            &rig,
            "tierstore_fs_manifest_fill_seconds_bucket{le=\"0.001\"}"
        ),
        1
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
    let second = reported(&rig);
    assert_eq!(second.attaches, [0, 1, 1]);
    assert_eq!(
        second.manifests,
        [first.manifests[0] + 2, 0, first.manifests[2]]
    );
    assert_eq!(second.timed, first.timed);
    assert!(rig.core.implicit().is_empty());
}
