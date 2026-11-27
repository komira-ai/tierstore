//! The read of a new tree through one `fill-tree` (`Core::read_tree`): a tree whose root manifest
//! the cache does not hold is filled by the cache in one request, which goes down the tree itself,
//! and the read then takes every manifest from the cache directory, so the upstream is asked once
//! for each manifest and the read asks for none; the tree read is the one its manifests flatten
//! to; a fill of the tree that fails leaves the manifests to the read, which fills them one at a
//! time and fails, or not, as it did before; the request carries the time left of the read's
//! bound, which bounds the read after it too; and the attach says the fill-tree and the levels on
//! stderr and reports the levels and rounds to the cache.
//!
//! The clock is the rig's, which only the test moves; the cache's own clock does not move.

use std::sync::Arc;
use std::time::Duration;

use super::super::{Errno, Looked};
use crate::server::treefs::ino;
use crate::server::treefs::testing::{
    Built, Logged, Rig, Upstream, dir, file, image_bytes, logged, sha,
};
use crate::server::treefs::tree::Tree;
use crate::tree_image_list::ListImage;
use crate::tree_manifest::{EntryKind, TreeDigest, TreeEntry, flatten_in_waves, parse_manifest};
use crate::{Clock, TreeImage};

/// A tree of `levels` manifests in a chain, each holding the next as `d` and a file of its own
/// beside it, with a sibling directory `s` at the root that is not part of the chain.
fn chain(levels: usize) -> Built {
    let mut node = dir(vec![("f", file(b"the bottom"))]);
    for level in 1..levels - 1 {
        let content = format!("level {level}").into_bytes();
        node = dir(vec![("d", node), ("f", file(&content))]);
    }
    Built::of(vec![("d", node), ("s", dir(vec![("g", file(b"beside"))]))])
}

/// The entries the rig's manifests of the tree `root` flatten to.
fn flattened(rig: &Rig, root: &str) -> Vec<TreeEntry> {
    let built = rig.upstream.built.lock().unwrap();
    let fetch = |digest: &TreeDigest, _: usize| -> Result<Vec<u8>, String> {
        let found = built.manifests.get(&digest.to_string());
        found
            .cloned()
            .ok_or_else(|| format!("no manifest {digest}"))
    };
    flatten_in_waves(&TreeDigest::from_hex(root).unwrap(), 16, fetch).unwrap()
}

/// The entries of the tree numbered `number` in the memory of `core`.
fn read(core: &super::super::Core, number: u64) -> Vec<TreeEntry> {
    let loaded = core.tree(number).unwrap();
    let count = u32::try_from(loaded.tree.len()).unwrap();
    (0..count)
        .map(|at| loaded.tree.entry(at).unwrap())
        .collect()
}

/// The value of the series `name` (with its labels) in the metrics of the node's cache, as
/// rendered: what the core reports reaches them at once, on the thread that reports it.
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

/// The fills of manifests the upstream was asked for, in order.
fn manifest_fills(rig: &Rig) -> Vec<String> {
    let fills = rig.upstream.asked("fill");
    fills
        .into_iter()
        .filter(|line| !line.contains(" from "))
        .collect()
}

/// How many of `fills` are fills of the manifest `digest`.
fn asked(fills: &[String], digest: &str) -> usize {
    let line = format!("fill {digest}");
    fills.iter().filter(|asked| **asked == line).count()
}

/// The digest of the first child of the manifest `digest` of the rig's upstream: the chain's.
fn first_child(rig: &Rig, digest: &str) -> String {
    let built = rig.upstream.built.lock().unwrap();
    let bytes = &built.manifests[digest];
    let manifest = parse_manifest(bytes, &TreeDigest::from_hex(digest).unwrap()).unwrap();
    let EntryKind::Dir { digest, .. } = &manifest.children[0].kind else {
        panic!("the chain's first child is a directory");
    };
    digest.to_string()
}

/// Lets every fill of a manifest go on when dropped: a test whose fills never come together ends
/// with the lookup answered, not waiting at the gate.
struct Opens(Arc<Upstream>);

impl Drop for Opens {
    fn drop(&mut self) {
        self.0.permit_manifests(1 << 20);
    }
}

/// The lines a lookup said of its attaches.
fn said(log: &Logged) -> Vec<String> {
    let text = log.text();
    let ours = |line: &&str| line.starts_with("tierstore fs: lookup of ");
    text.lines().filter(ours).map(str::to_owned).collect()
}

/// Asserts that a lookup of a new tree whose new manifests are a chain of 6 levels has the cache
/// fill them in one request: the upstream is asked once for each of the 7 manifests (as the
/// cache's fill of the tree) and the read asks for none, where a read a level at a time asks one
/// fill for each level; that the tree read is the one its manifests flatten to; and that the
/// attach says the fill-tree and the 6 levels, all read from the cache, and reports 6 levels and 1
/// round.
#[test]
fn a_new_tree_is_filled_by_one_request_and_read_from_the_cache() {
    let rig = Rig::new("tfs-tree-fill", chain(6));
    let root = rig.upstream.built.lock().unwrap().root.clone();
    let log = Logged::default();
    let found = {
        let _logging = logged(&log, tracing::Level::INFO);
        rig.core.lookup(ino::TREES, &root)
    };
    assert!(matches!(found, Ok(Looked::Found(_))), "{found:?}");
    let mut fills = manifest_fills(&rig);
    assert_eq!(fills.len(), 7);
    fills.sort();
    fills.dedup();
    assert_eq!(fills.len(), 7, "a manifest was asked for twice");
    assert_eq!(
        counted(&rig, "tierstore_cache_manifest_fill_seconds_count{via=\"tree\"}"),
        7
    );
    assert_eq!(
        counted(&rig, "tierstore_cache_manifest_fill_seconds_count{via=\"fill\"}"),
        0
    );
    assert_eq!(read(&rig.core, 1), flattened(&rig, &root));
    assert_eq!(counted(&rig, "tierstore_fs_tree_levels_total"), 6);
    assert_eq!(counted(&rig, "tierstore_fs_tree_fill_rounds_total"), 1);
    let line = format!(
        "tierstore fs: lookup of {root}: ok in 0 ms; 7 manifests: 7 from the cache, 0 filled, 0 \
         not had; 6 levels; fills by level: 0 0 0 0 0 0, slowest ms by level: 0 0 0 0 0 0; \
         fill-tree ok in 0 ms, 0 ms at the node, fills by level: 1 2 1 1 1 1"
    );
    assert_eq!(said(&log), [line]);
}

/// Asserts that a lookup of a tree whose root manifest the cache holds asks no `fill-tree`: the
/// second lookup, by another core over the cache the first filled, reads every manifest from the
/// cache with nothing asked of the upstream, and reports 6 levels and no round.
#[test]
fn a_tree_whose_root_the_cache_holds_asks_no_fill_tree() {
    let rig = Rig::new("tfs-tree-fill-held", chain(6));
    let root = rig.upstream.built.lock().unwrap().root.clone();
    assert!(matches!(
        rig.core.lookup(ino::TREES, &root),
        Ok(Looked::Found(_))
    ));
    let fills = rig.upstream.asked("fill").len();
    let rounds = counted(&rig, "tierstore_fs_tree_fill_rounds_total");
    let levels = counted(&rig, "tierstore_fs_tree_levels_total");
    let second = rig.second_core();
    assert!(matches!(
        second.lookup(ino::TREES, &root),
        Ok(Looked::Found(_))
    ));
    assert_eq!(counted(&rig, "tierstore_fs_tree_levels_total"), levels + 6);
    assert_eq!(counted(&rig, "tierstore_fs_tree_fill_rounds_total"), rounds);
    assert_eq!(rig.upstream.asked("fill").len(), fills);
    assert_eq!(
        counted(&rig, "tierstore_cache_manifest_fill_seconds_count{via=\"tree\"}"),
        7
    );
}

/// Asserts that a child the upstream does not have fails the attach as before, `EIO`, and a root
/// it does not have is `ENOENT`, as before: the cache's fill of the tree fails at the missing
/// manifest, and the read asks for it once more itself, with what else of its level it did not
/// find in the cache (not the root, which the cache filled), and fails in its own words.
#[test]
fn a_missing_child_or_root_fails_the_attach_as_before() {
    let rig = Rig::new("tfs-tree-fill-missing", chain(4));
    let root = rig.upstream.built.lock().unwrap().root.clone();
    let child = first_child(&rig, &root);
    let _ = rig.upstream.built.lock().unwrap().manifests.remove(&child);
    assert_eq!(rig.core.lookup(ino::TREES, &root), Err(Errno::Io));
    let fills = manifest_fills(&rig);
    assert_eq!(asked(&fills, &child), 2, "{fills:?}");
    assert_eq!(asked(&fills, &root), 1, "{fills:?}");
    assert!(rig.core.implicit().is_empty());

    let absent = sha(b"a tree nobody wrote");
    assert_eq!(rig.core.lookup(ino::TREES, &absent), Err(Errno::NoEnt));
    let fills = manifest_fills(&rig);
    assert_eq!(asked(&fills, &absent), 2, "{fills:?}");
    assert_eq!(fills.last(), Some(&format!("fill {absent}")));
}

/// Asserts that a fill of the tree that fails leaves the manifests to the read, which fills them
/// one at a time and attaches the tree: the upstream refuses a directory of the first level (a
/// 503) until the read asks for it itself; the failure is said on stderr as a failed fill-tree,
/// with the manifest it failed at, and the read has the cache fill what the fill of the tree did
/// not.
#[test]
fn a_fill_tree_that_fails_leaves_the_manifests_to_the_read() {
    let rig = Rig::new("tfs-tree-fill-failed", chain(3));
    let root = rig.upstream.built.lock().unwrap().root.clone();
    let child = first_child(&rig, &root);
    let _ = rig
        .upstream
        .failing
        .lock()
        .unwrap()
        .insert(child.clone(), "upstream 503".to_owned());
    rig.upstream.gate_manifests();
    let log = Logged::default();
    let found = std::thread::scope(|scope| {
        let upstream = Arc::clone(&rig.upstream);
        let _driver = scope.spawn(move || {
            let opens = Opens(upstream);
            let up = &opens.0;
            // The root, then the two directories of the first level, which fail or not as they
            // are set to.
            up.permit_manifests(1);
            up.wait_for_manifest_fills(3);
            up.permit_manifests(2);
            // The fill of the tree has failed: the next fill is the read's own, for the
            // directory the fill of the tree failed at. The upstream gives it now.
            up.wait_for_manifest_fills(4);
            let _ = up.failing.lock().unwrap().remove(&child);
        });
        let _logging = logged(&log, tracing::Level::INFO);
        rig.core.lookup(ino::TREES, &root)
    });
    assert!(matches!(found, Ok(Looked::Found(_))), "{found:?}");
    assert_eq!(read(&rig.core, 1), flattened(&rig, &root));
    assert!(
        counted(&rig, "tierstore_cache_manifest_fill_seconds_count{via=\"fill\"}") >= 1,
        "the read filled nothing itself"
    );
    let [line] = &said(&log)[..] else {
        panic!("one line: {:?}", said(&log));
    };
    let child = first_child(&rig, &root);
    assert!(
        line.ends_with(&format!(
            "; fill-tree failed in 0 ms: upstream 503 at {child}"
        )),
        "{line}"
    );
}

/// Asserts that the read's one bound holds across the fill-tree and the read after it: a fill of
/// the tree that takes the whole 30 s of the bound on the rig's clock and fills nothing leaves the
/// read no time, so the lookup is `EIO` with no fill of a manifest asked after it, and the
/// fill-tree is said as failed in 30000 ms.
#[test]
fn a_fill_tree_that_takes_the_bound_leaves_the_read_none() {
    let rig = Rig::new("tfs-tree-fill-bound", chain(3));
    let root = rig.upstream.built.lock().unwrap().root.clone();
    let _ = rig
        .upstream
        .failing
        .lock()
        .unwrap()
        .insert(root.clone(), "upstream 503".to_owned());
    rig.upstream.gate_manifests();
    let log = Logged::default();
    let found = std::thread::scope(|scope| {
        let upstream = Arc::clone(&rig.upstream);
        let clock = Arc::clone(&rig.clock);
        let _driver = scope.spawn(move || {
            let opens = Opens(upstream);
            // The root's fill is received, and the whole bound goes by before it is answered.
            opens.0.wait_for_manifest_fills(1);
            clock.advance(Duration::from_secs(30));
        });
        let _logging = logged(&log, tracing::Level::INFO);
        rig.core.lookup(ino::TREES, &root)
    });
    assert_eq!(found, Err(Errno::Io));
    assert_eq!(manifest_fills(&rig), [format!("fill {root}")]);
    assert_eq!(rig.clock.now(), 30_000);
    let [line] = &said(&log)[..] else {
        panic!("one line: {:?}", said(&log));
    };
    assert!(
        line.ends_with(&format!(
            "; fill-tree failed in 30000 ms: upstream 503 at {root}"
        )),
        "{line}"
    );
}

/// The entries, by index, of the image of the version 2 tree list `bytes`, as a tree of `trees/`.
fn of_the_image(bytes: &[u8]) -> Vec<TreeEntry> {
    let image = TreeImage::checked(bytes.to_vec().into()).unwrap();
    let tree = Tree::image(Arc::new(ListImage::heap(&image)), None);
    let count = u32::try_from(tree.len()).unwrap();
    (0..count).map(|at| tree.entry(at).unwrap()).collect()
}

/// Puts the version 2 tree list of [`image_bytes`] in the rig's upstream as the object
/// `cas/l/<its digest>`, where a root manifest would be; its bytes and its digest.
fn list_in(rig: &Rig) -> (Vec<u8>, String) {
    let bytes = image_bytes();
    let digest = sha(&bytes);
    let mut built = rig.upstream.built.lock().unwrap();
    let _ = built.manifests.insert(digest.clone(), bytes.clone());
    (bytes, digest)
}

/// Asserts that a lookup of `trees/<digest>` whose object is a version 2 tree list is attached from
/// the one fill of that object: the cache's fill of the tree stops at the list it fills, and the
/// read fills nothing itself and reads no manifest (the attach says 0 manifests and the one level
/// of the fill-tree); the tree attached is the list, mapped once, with no location, its entries by
/// index those of the list's image (the root, then `bin`, `d`, `empty`, `link`, `bin/other` and
/// `bin/tool`); and its entries are looked up with the modes the list stores (`d` 0700, where a
/// tree of manifests shows every directory 0755) and the sizes it says.
#[test]
fn a_tree_list_at_trees_digest_is_attached_from_one_fill() {
    let rig = Rig::new("tfs-tree-list", Built::of(Vec::new()));
    let (bytes, list) = list_in(&rig);
    let log = Logged::default();
    let found = {
        let _logging = logged(&log, tracing::Level::INFO);
        rig.core.lookup(ino::TREES, &list)
    };
    let Ok(Looked::Found(root)) = &found else {
        panic!("{found:?}");
    };
    assert_eq!(manifest_fills(&rig), [format!("fill {list}")]);
    assert_eq!(
        counted(&rig, "tierstore_cache_manifest_fill_seconds_count{via=\"fill\"}"),
        0
    );
    let entries = read(&rig.core, 1);
    assert_eq!(entries, of_the_image(&bytes));
    let names: Vec<&str> = entries.iter().map(|entry| &*entry.name).collect();
    assert_eq!(names, ["", "bin", "d", "empty", "link", "other", "tool"]);
    let loaded = rig.core.tree(1).unwrap();
    assert!(loaded.tree.list().is_some());
    assert_eq!(loaded.tree.location(), None);
    assert_eq!(rig.core.lists.mapped.mapped().lists, 1);
    let Ok(Looked::Found(d)) = rig.core.lookup(root.attr.ino, "d") else {
        panic!("d");
    };
    assert_eq!(d.attr.perm, 0o700);
    let Ok(Looked::Found(bin)) = rig.core.lookup(root.attr.ino, "bin") else {
        panic!("bin");
    };
    let Ok(Looked::Found(tool)) = rig.core.lookup(bin.attr.ino, "tool") else {
        panic!("bin/tool");
    };
    assert_eq!((tool.attr.perm, tool.attr.size), (0o755, 5));
    assert_eq!(counted(&rig, "tierstore_fs_tree_levels_total"), 1);
    assert_eq!(counted(&rig, "tierstore_fs_tree_fill_rounds_total"), 1);
    let line = format!(
        "tierstore fs: lookup of {list}: ok in 0 ms; 0 manifests: 0 from the cache, 0 filled, 0 \
         not had; 1 levels; fills by level: 0, slowest ms by level: 0; fill-tree ok in 0 ms, 0 ms \
         at the node, fills by level: 1; a tree list of {} bytes",
        bytes.len()
    );
    assert_eq!(said(&log), [line]);
}

/// Asserts that the new reader takes a manifest root at `trees/<digest>` on the manifest path, as
/// before (B8, the reader of this release on the data of the release before): a tree of manifests
/// is flattened from its manifests, is no list, maps nothing, and its root's manifest is counted.
#[test]
fn a_manifest_root_at_trees_digest_takes_the_manifest_path() {
    let rig = Rig::new("tfs-tree-manifest-root", chain(2));
    let root = rig.upstream.built.lock().unwrap().root.clone();
    assert!(matches!(
        rig.core.lookup(ino::TREES, &root),
        Ok(Looked::Found(_))
    ));
    assert_eq!(read(&rig.core, 1), flattened(&rig, &root));
    assert!(rig.core.tree(1).unwrap().tree.list().is_none());
    assert_eq!(rig.core.lists.mapped.mapped().lists, 0);
    assert_eq!(
        counted(&rig, "tierstore_fs_manifest_fetch_total{result=\"cache\"}"),
        3
    );
}
