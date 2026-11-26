//! The read of a new tree through one `fill-tree` (`Core::read_tree`), as the fill socket sees it:
//! a tree whose root manifest the cache does not hold is filled by the node in one request, and the
//! read then takes every manifest from the cache directory, so the socket is crossed once for the
//! tree and not once for each level; the tree read is the one its manifests flatten to; a node of a
//! release before the request, or one whose fill fails, leaves the manifests to the read, which
//! fills them one at a time and fails, or not, as it did before; the request carries the time left
//! of the read's bound, which bounds the read after it too; and the attach says the fill-tree and
//! the levels on stderr and reports the levels and rounds to the node.
//!
//! The node is a socket of the test's own, which answers `fill-tree` as a node of this release does
//! (from the rig's manifests, into the rig's cache directory) and every other line as the rig's fake
//! does; the clock is the rig's, which only the test moves.

use std::io::{BufRead as _, BufReader, Write as _};
use std::os::unix::net::UnixListener;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use super::super::{Errno, Looked};
use super::tree_read_tests::core_over;
use crate::server::local_cache::{TreeFilled, manifest_path};
use crate::server::treefs::ino;
use crate::server::treefs::testing::{
    Built, Logged, Rig, Upstream, dir, file, image_bytes, logged, sha, until,
};
use crate::server::treefs::tree::Tree;
use crate::tree_image_list::ListImage;
use crate::tree_manifest::{EntryKind, TreeDigest, TreeEntry, flatten_in_waves, parse_manifest};
use crate::{Clock, TreeImage, is_tree_list_v2};

/// What answers a `fill-tree`: the rig's fake, the digest of the root, and the line asked.
type Answer = Arc<dyn Fn(&Upstream, &str, &str) -> String + Send + Sync>;

/// The node of a test: the lines its socket received, in order.
struct Node {
    lines: Arc<Mutex<Vec<String>>>,
    socket: PathBuf,
}

impl Node {
    /// A socket in the rig's directory that answers `fill-tree` by `answer` and every other line
    /// as the rig's fake does.
    fn start(rig: &Rig, name: &str, answer: Answer) -> Self {
        let socket = rig.dir.path().join(name);
        let listener = UnixListener::bind(&socket).unwrap();
        let lines = Arc::new(Mutex::new(Vec::new()));
        let (seen, upstream) = (Arc::clone(&lines), Arc::clone(&rig.upstream));
        let _ = std::thread::spawn(move || {
            for stream in listener.incoming().flatten() {
                let (seen, upstream, answer) = (
                    Arc::clone(&seen),
                    Arc::clone(&upstream),
                    Arc::clone(&answer),
                );
                let _ = std::thread::spawn(move || {
                    let mut reader = BufReader::new(&stream);
                    let mut line = String::new();
                    while reader.read_line(&mut line).unwrap_or(0) > 0 {
                        let text = line.trim_end_matches('\n').to_owned();
                        line.clear();
                        seen.lock().unwrap().push(text.clone());
                        let said = match text.strip_prefix("fill-tree ") {
                            Some(rest) => {
                                let root = rest.split(' ').next().unwrap_or_default();
                                Some(answer(&upstream, root, &text))
                            }
                            None => upstream.answer(&text),
                        };
                        if let Some(said) = said {
                            (&stream).write_all(format!("{said}\n").as_bytes()).unwrap();
                        }
                    }
                });
            }
        });
        Self { lines, socket }
    }

    /// The lines received whose first word is `verb`; for `fill`, the fills of manifests only.
    fn asked(&self, verb: &str) -> Vec<String> {
        let lines = self.lines.lock().unwrap();
        lines
            .iter()
            .filter(|line| line.split(' ').next() == Some(verb))
            .filter(|line| verb != "fill" || !line.contains(" from "))
            .cloned()
            .collect()
    }
}

/// The answer of a node of this release to `fill-tree <root>`: the manifests of the rig's
/// upstream the cache directory `cache` does not hold, put there a level at a time from the root,
/// a directory the cache holds not gone into, as `local_cache/tree_fill.rs` does; `ok 3`
/// and the manifests filled at each level, or the words of the first that the upstream does not
/// have. A root that is a version 2 tree list is put there and gone no further into, `ok 3 1`.
fn descend(upstream: &Upstream, cache: &Path, root: &str) -> String {
    let built = upstream.built.lock().unwrap();
    if let Some(list) = built.manifests.get(root).filter(|b| is_tree_list_v2(b)) {
        let path = manifest_path(cache, root);
        let filled = u64::from(!path.exists());
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, list).unwrap();
        return TreeFilled {
            ms: 3,
            levels: vec![filled],
        }
        .answer();
    }
    let mut levels = Vec::new();
    let mut level = vec![root.to_owned()];
    while !level.is_empty() {
        let (mut below, mut filled) = (Vec::new(), 0);
        for sha in &level {
            let path = manifest_path(cache, sha);
            let held = path.exists();
            if held && !levels.is_empty() {
                continue;
            }
            let Some(bytes) = built.manifests.get(sha) else {
                return format!("upstream 404 at {sha}");
            };
            if !held {
                std::fs::create_dir_all(path.parent().unwrap()).unwrap();
                std::fs::write(&path, bytes).unwrap();
                filled += 1;
            }
            let manifest = parse_manifest(bytes, &TreeDigest::from_hex(sha).unwrap()).unwrap();
            for child in &manifest.children {
                if let EntryKind::Dir { digest, .. } = &child.kind {
                    below.push(digest.to_string());
                }
            }
        }
        levels.push(filled);
        level = below;
    }
    TreeFilled { ms: 3, levels }.answer()
}

/// A node of this release over the rig's cache directory.
fn this_release(rig: &Rig) -> Answer {
    let cache = rig.cache.clone();
    Arc::new(move |upstream, root, _| descend(upstream, &cache, root))
}

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

/// The reports of `verb` the rig's fake was sent, once `count` of them have come: they are sent
/// by a thread of their own.
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

/// Asserts that a lookup of a new tree whose new manifests are a chain of 6 levels crosses the
/// fill socket once for them: one `fill-tree` of the root, with the 30 s of the read's bound, and
/// no fill of a manifest, where a read a level at a time asks one fill for each level; that the
/// tree read is the one its manifests flatten to; and that the attach says the fill-tree and the 6
/// levels, all read from the cache, and reports 6 levels and 1 round.
#[test]
fn a_new_tree_is_filled_by_one_request_and_read_from_the_cache() {
    let rig = Rig::new("tfs-tree-fill", chain(6));
    let root = rig.upstream.built.lock().unwrap().root.clone();
    let node = Node::start(&rig, "node.sock", this_release(&rig));
    let core = core_over(&rig, &node.socket, |_| {});
    let log = Logged::default();
    let found = {
        let _logging = logged(&log, tracing::Level::INFO);
        core.lookup(ino::TREES, &root)
    };
    assert!(matches!(found, Ok(Looked::Found(_))), "{found:?}");
    assert_eq!(node.asked("fill-tree"), [format!("fill-tree {root} 30000")]);
    assert_eq!(node.asked("fill"), Vec::<String>::new());
    assert_eq!(read(&core, 1), flattened(&rig, &root));
    assert_eq!(reports(&rig, "levels", 1), ["levels 6 1"]);
    let line = format!(
        "tierstore fs: lookup of {root}: ok in 0 ms; 7 manifests: 7 from the cache, 0 filled, 0 \
         not had; 6 levels; fills by level: 0 0 0 0 0 0, slowest ms by level: 0 0 0 0 0 0; \
         fill-tree ok in 0 ms, 3 ms at the node, fills by level: 1 2 1 1 1 1"
    );
    assert_eq!(said(&log), [line]);
}

/// Asserts that a node of a release before `fill-tree` makes the read fill the manifests one at a
/// time, as it did before the request: the rig's own fake refuses it as unknown, and the read asks
/// one fill for each of the 7 manifests of the chain, reads the same tree, says the fill-tree was
/// unknown to the node, and reports 6 levels and 7 rounds (6 levels of fills, and the request).
#[test]
fn a_node_without_the_request_has_the_tree_read_one_manifest_at_a_time() {
    let rig = Rig::new("tfs-tree-fill-old", chain(6));
    let root = rig.upstream.built.lock().unwrap().root.clone();
    let log = Logged::default();
    let found = {
        let _logging = logged(&log, tracing::Level::INFO);
        rig.core.lookup(ino::TREES, &root)
    };
    assert!(matches!(found, Ok(Looked::Found(_))), "{found:?}");
    assert_eq!(rig.upstream.asked("fill-tree").len(), 1);
    let fills = rig.upstream.asked("fill");
    assert_eq!(fills.iter().filter(|l| !l.contains(" from ")).count(), 7);
    assert_eq!(read(&rig.core, 1), flattened(&rig, &root));
    assert_eq!(reports(&rig, "levels", 1), ["levels 6 7"]);
    let [line] = &said(&log)[..] else {
        panic!("one line: {:?}", said(&log));
    };
    assert!(
        line.ends_with(
            "; 6 levels; fills by level: 1 2 1 1 1 1, slowest ms by level: 0 0 0 0 0 0; \
             fill-tree unknown to the node in 0 ms"
        ),
        "{line}"
    );
}

/// Asserts that a lookup of a tree whose root manifest the cache holds asks no `fill-tree`: the
/// second lookup, by another core over the cache the first filled, reads every manifest from the
/// cache with nothing asked of the socket, and reports 6 levels and no round.
#[test]
fn a_tree_whose_root_the_cache_holds_asks_no_fill_tree() {
    let rig = Rig::new("tfs-tree-fill-held", chain(6));
    let root = rig.upstream.built.lock().unwrap().root.clone();
    let node = Node::start(&rig, "node.sock", this_release(&rig));
    let first = core_over(&rig, &node.socket, |_| {});
    assert!(matches!(
        first.lookup(ino::TREES, &root),
        Ok(Looked::Found(_))
    ));
    let _ = reports(&rig, "levels", 1);
    let second = rig.second_core();
    assert!(matches!(
        second.lookup(ino::TREES, &root),
        Ok(Looked::Found(_))
    ));
    assert_eq!(reports(&rig, "levels", 2)[1], "levels 6 0");
    assert_eq!(node.asked("fill-tree").len(), 1);
    assert_eq!(rig.upstream.asked("fill-tree").len(), 0);
    assert_eq!(rig.upstream.asked("fill").len(), 0);
}

/// Asserts that a child the upstream does not have fails the attach as before, `EIO`, and a root
/// it does not have is `ENOENT`, as before: the node answers the fill-tree with the words of the
/// missing manifest, and the read asks for it once more itself, with what else of its level it did
/// not find in the cache (not the root, which the node filled), and fails in its own words.
#[test]
fn a_missing_child_or_root_fails_the_attach_as_before() {
    let rig = Rig::new("tfs-tree-fill-missing", chain(4));
    let (root, child) = {
        let mut built = rig.upstream.built.lock().unwrap();
        let root = built.root.clone();
        let bytes = &built.manifests[&root];
        let manifest = parse_manifest(bytes, &TreeDigest::from_hex(&root).unwrap()).unwrap();
        let EntryKind::Dir { digest, .. } = &manifest.children[0].kind else {
            panic!("the chain's first child is a directory");
        };
        let child = digest.to_string();
        built.manifests.remove(&child);
        (root, child)
    };
    let node = Node::start(&rig, "node.sock", this_release(&rig));
    let core = core_over(&rig, &node.socket, |_| {});
    assert_eq!(core.lookup(ino::TREES, &root), Err(Errno::Io));
    assert_eq!(node.asked("fill-tree").len(), 1);
    let fills = node.asked("fill");
    assert!(fills.contains(&format!("fill {child}")), "{fills:?}");
    assert!(!fills.contains(&format!("fill {root}")), "{fills:?}");
    assert!(core.implicit().is_empty());

    let absent = sha(b"a tree nobody wrote");
    assert_eq!(core.lookup(ino::TREES, &absent), Err(Errno::NoEnt));
    assert_eq!(node.asked("fill-tree").len(), 2);
    assert_eq!(node.asked("fill").last(), Some(&format!("fill {absent}")));
}

/// Asserts that a node that fills none of the new manifests leaves them all to the read, which
/// fills them one at a time and attaches the tree: a node that answers `busy`, and one whose answer
/// is not one of a fill-tree (`ok` alone, `ok x`); each is said on stderr as a failed fill-tree.
#[test]
fn a_fill_tree_that_fails_leaves_the_manifests_to_the_read() {
    for (at, answer) in ["busy", "ok", "ok 1 x"].into_iter().enumerate() {
        let rig = Rig::new(&format!("tfs-tree-fill-failed-{at}"), chain(3));
        let root = rig.upstream.built.lock().unwrap().root.clone();
        let words = answer.to_owned();
        let node = Node::start(&rig, "node.sock", Arc::new(move |_, _, _| words.clone()));
        let core = core_over(&rig, &node.socket, |_| {});
        let log = Logged::default();
        let found = {
            let _logging = logged(&log, tracing::Level::INFO);
            core.lookup(ino::TREES, &root)
        };
        assert!(matches!(found, Ok(Looked::Found(_))), "{answer}: {found:?}");
        assert_eq!(node.asked("fill").len(), 4, "{answer}");
        assert_eq!(read(&core, 1), flattened(&rig, &root));
        let [line] = &said(&log)[..] else {
            panic!("one line: {:?}", said(&log));
        };
        assert!(
            line.ends_with(&format!("; fill-tree failed in 0 ms: {answer}")),
            "{line}"
        );
    }
}

/// Asserts that the read's one bound holds across the fill-tree and the read after it: a node
/// that takes the whole 30 s of the bound on the rig's clock and fills nothing leaves the read no
/// time, so the lookup is `EIO` with no fill of a manifest asked, and the fill-tree is said as `ok`
/// in 30000 ms.
#[test]
fn a_fill_tree_that_takes_the_bound_leaves_the_read_none() {
    let rig = Rig::new("tfs-tree-fill-bound", chain(3));
    let root = rig.upstream.built.lock().unwrap().root.clone();
    let clock = Arc::clone(&rig.clock);
    let slow: Answer = Arc::new(move |_, _, _| {
        clock.advance(Duration::from_secs(30));
        "ok 30000".to_owned()
    });
    let node = Node::start(&rig, "node.sock", slow);
    let core = core_over(&rig, &node.socket, |_| {});
    let log = Logged::default();
    let found = {
        let _logging = logged(&log, tracing::Level::INFO);
        core.lookup(ino::TREES, &root)
    };
    assert_eq!(found, Err(Errno::Io));
    assert_eq!(node.asked("fill"), Vec::<String>::new());
    assert_eq!(rig.clock.now(), 30_000);
    let [line] = &said(&log)[..] else {
        panic!("one line: {:?}", said(&log));
    };
    assert!(
        line.ends_with("; fill-tree ok in 30000 ms, 30000 ms at the node, fills by level:"),
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
/// the one fill of that object: the node is asked one `fill-tree`, which stops at the list it
/// fills, and the read fills nothing itself and reads no manifest (the attach says 0 manifests and
/// the one level of the fill-tree); the tree attached is the list, mapped once, with no location,
/// its entries by index those of the list's image (the root, then `bin`, `d`, `empty`, `link`,
/// `bin/other` and `bin/tool`); and its entries are looked up with the modes the list stores (`d`
/// 0700, where a tree of manifests shows every directory 0755) and the sizes it says.
#[test]
fn a_tree_list_at_trees_digest_is_attached_from_one_fill() {
    let rig = Rig::new("tfs-tree-list", Built::of(Vec::new()));
    let (bytes, list) = list_in(&rig);
    let node = Node::start(&rig, "node.sock", this_release(&rig));
    let core = core_over(&rig, &node.socket, |_| {});
    let log = Logged::default();
    let found = {
        let _logging = logged(&log, tracing::Level::INFO);
        core.lookup(ino::TREES, &list)
    };
    let Ok(Looked::Found(root)) = &found else {
        panic!("{found:?}");
    };
    assert_eq!(node.asked("fill-tree"), [format!("fill-tree {list} 30000")]);
    assert_eq!(node.asked("fill"), Vec::<String>::new());
    let entries = read(&core, 1);
    assert_eq!(entries, of_the_image(&bytes));
    let names: Vec<&str> = entries.iter().map(|entry| &*entry.name).collect();
    assert_eq!(names, ["", "bin", "d", "empty", "link", "other", "tool"]);
    let loaded = core.tree(1).unwrap();
    assert!(loaded.tree.list().is_some());
    assert_eq!(loaded.tree.location(), None);
    assert_eq!(core.lists.mapped.mapped().lists, 1);
    let Ok(Looked::Found(d)) = core.lookup(root.attr.ino, "d") else {
        panic!("d");
    };
    assert_eq!(d.attr.perm, 0o700);
    let Ok(Looked::Found(bin)) = core.lookup(root.attr.ino, "bin") else {
        panic!("bin");
    };
    let Ok(Looked::Found(tool)) = core.lookup(bin.attr.ino, "tool") else {
        panic!("bin/tool");
    };
    assert_eq!((tool.attr.perm, tool.attr.size), (0o755, 5));
    assert_eq!(reports(&rig, "levels", 1), ["levels 1 1"]);
    let line = format!(
        "tierstore fs: lookup of {list}: ok in 0 ms; 0 manifests: 0 from the cache, 0 filled, 0 \
         not had; 1 levels; fills by level: 0, slowest ms by level: 0; fill-tree ok in 0 ms, 3 ms \
         at the node, fills by level: 1; a tree list of {} bytes",
        bytes.len()
    );
    assert_eq!(said(&log), [line]);
}

/// Asserts that a tree list a node of the release before fills is read from the cache all the
/// same: that node fills the root of a `fill-tree` and then answers that it cannot read it as a
/// manifest; the read finds the list in the cache, fills nothing itself, and attaches the list.
#[test]
fn a_tree_list_a_node_of_the_release_before_fills_is_read_from_the_cache() {
    let rig = Rig::new("tfs-tree-list-old", Built::of(Vec::new()));
    let (bytes, list) = list_in(&rig);
    let cache = rig.cache.clone();
    let before: Answer = Arc::new(move |upstream, root, _| {
        let path = manifest_path(&cache, root);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, &upstream.built.lock().unwrap().manifests[root]).unwrap();
        format!("cannot read {root}: malformed manifest: expected value at line 1 column 1")
    });
    let node = Node::start(&rig, "node.sock", before);
    let core = core_over(&rig, &node.socket, |_| {});
    assert!(matches!(
        core.lookup(ino::TREES, &list),
        Ok(Looked::Found(_))
    ));
    assert_eq!(node.asked("fill-tree").len(), 1);
    assert_eq!(node.asked("fill"), Vec::<String>::new());
    assert_eq!(read(&core, 1), of_the_image(&bytes));
}

/// Asserts that the new reader takes a manifest root at `trees/<digest>` on the manifest path, as
/// before (B8, the reader of this release on the data of the release before): a tree of manifests
/// is flattened from its manifests, is no list, maps nothing, and its root's manifest is counted.
#[test]
fn a_manifest_root_at_trees_digest_takes_the_manifest_path() {
    let rig = Rig::new("tfs-tree-manifest-root", chain(2));
    let root = rig.upstream.built.lock().unwrap().root.clone();
    let node = Node::start(&rig, "node.sock", this_release(&rig));
    let core = core_over(&rig, &node.socket, |_| {});
    assert!(matches!(
        core.lookup(ino::TREES, &root),
        Ok(Looked::Found(_))
    ));
    assert_eq!(read(&core, 1), flattened(&rig, &root));
    assert!(core.tree(1).unwrap().tree.list().is_none());
    assert_eq!(core.lists.mapped.mapped().lists, 0);
    assert_eq!(reports(&rig, "fetched", 1)[0].split(' ').nth(1), Some("3"));
}
