//! The flattening of a tree a level at a time: the entries `flatten` makes, for every shape and
//! width; each manifest fetched once; a level fetched at once, so that a tree costs a round of
//! fetches for each level; the width that bounds a level; every refusal and limit as `flatten`
//! has it, with no more fetched; and a fetch that panics on a helper thread.

use std::collections::HashMap;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use super::{HELPER, flatten_in_waves, gather};
use crate::tree_manifest::{EntryKind, TreeDigest, TreeEntry, TreeError, flatten, parse_manifest};
use crate::tree_manifest_tests::{dir, file, link, manifest};

/// The width of the tests that are not about the width: that of the mount.
const WIDTH: usize = 16;

/// How many times a fetch that waits for the rest of its level looks before it gives up: far more
/// than threads that are all running need, so it gives up only when they never come.
const LOOKS: u32 = 10_000_000;

/// Manifests by digest, and how many times each was asked for; it may be asked from several
/// threads at once.
#[derive(Default)]
struct Store {
    blobs: HashMap<TreeDigest, Vec<u8>>,
    asked: Mutex<HashMap<TreeDigest, usize>>,
}

impl Store {
    fn put(&mut self, text: String) -> TreeDigest {
        let digest = TreeDigest::of(text.as_bytes());
        self.blobs.insert(digest, text.into_bytes());
        digest
    }

    fn get(&self, digest: &TreeDigest) -> Result<Vec<u8>, String> {
        *self.asked.lock().unwrap().entry(*digest).or_default() += 1;
        self.blobs
            .get(digest)
            .cloned()
            .ok_or_else(|| format!("no object {digest}"))
    }

    /// The fetches made since the last look, in all, and the most of any one manifest; the count
    /// starts again.
    fn take_asked(&self) -> (usize, usize) {
        let asked = std::mem::take(&mut *self.asked.lock().unwrap());
        let most = asked.values().copied().max().unwrap_or(0);
        (asked.values().sum(), most)
    }

    fn serial(&self, root: &TreeDigest) -> Result<Vec<TreeEntry>, TreeError> {
        flatten(root, |digest| self.get(digest))
    }

    fn waves(&self, root: &TreeDigest, width: usize) -> Result<Vec<TreeEntry>, TreeError> {
        flatten_in_waves(root, width, |digest| self.get(digest))
    }

    /// A chain of `levels` manifests, each holding one directory `d` that is the next, the last
    /// holding the file `f` or nothing.
    fn chain(&mut self, levels: usize, bottom_file: bool) -> TreeDigest {
        let bottom = if bottom_file {
            manifest(&[file("f", 1)])
        } else {
            manifest(&[])
        };
        let mut top = self.put(bottom);
        for _ in 1..levels {
            top = self.put(manifest(&[dir("d", &top)]));
        }
        top
    }

    /// A tree of `levels` levels of manifests, each but the last holding a file named by its path,
    /// so that every manifest is distinct, and two directories: 2^levels - 1 manifests, the level
    /// `k` (the root's is 0) of 2^k.
    fn binary(&mut self, levels: usize, path: &str) -> TreeDigest {
        // The file `f...` sorts before `l` and `r`.
        let mut children = vec![file(&format!("f{path}"), 1)];
        if levels > 1 {
            for side in ["l", "r"] {
                let child = self.binary(levels - 1, &format!("{path}{side}"));
                children.push(dir(side, &child));
            }
        }
        self.put(manifest(&children))
    }

    /// The directories of the manifest `digest`.
    fn dirs(&self, digest: &TreeDigest) -> Vec<TreeDigest> {
        let parsed = parse_manifest(&self.blobs[digest], digest).unwrap();
        let mut dirs = Vec::new();
        for child in parsed.children {
            if let EntryKind::Dir { digest, .. } = child.kind {
                dirs.push(digest);
            }
        }
        dirs
    }
}

/// A tree with every kind of entry, an empty directory, and one subtree listed at depth 1 and twice
/// at depth 2 (once below a directory of the same level as itself): 5 distinct manifests.
fn mixed(store: &mut Store) -> TreeDigest {
    let shared = store.put(manifest(&[file("g", 2)]));
    let empty = store.put(manifest(&[]));
    let first = store.put(manifest(&[
        file("f", 1),
        link("l", "../c"),
        dir("s", &shared),
    ]));
    let second = store.put(manifest(&[dir("x", &shared)]));
    store.put(manifest(&[
        dir("a", &first),
        dir("b", &shared),
        file("c", 3),
        dir("d", &empty),
        dir("e", &second),
    ]))
}

/// Asserts that the entries are those of `flatten`, index for index, for a tree with every kind of
/// entry and a subtree shared across levels, an empty tree and a chain, at the widths 1, 2 and 16;
/// and that each distinct manifest is fetched once.
#[test]
fn the_entries_are_those_of_flatten_and_each_manifest_is_fetched_once() {
    let mut store = Store::default();
    let mixed = mixed(&mut store);
    let empty = store.put(manifest(&[]));
    let chain = store.chain(50, true);
    for (root, distinct) in [(mixed, 5), (empty, 1), (chain, 50)] {
        let serial = store.serial(&root).unwrap();
        let _ = store.take_asked();
        for width in [1, 2, WIDTH] {
            assert_eq!(store.waves(&root, width).unwrap(), serial, "width {width}");
            assert_eq!(store.take_asked(), (distinct, 1), "width {width}");
        }
    }
    assert_eq!(store.serial(&mixed).unwrap().len(), 13);
}

/// A fetch that holds each call until the manifests of its level that may be fetched at once are
/// all being fetched, as an upstream whose every answer takes the same time holds them, and counts
/// the rounds: the times a fetch began with no other in flight. With no clock: a fetch waits by
/// looking, and one that waits in vain (its level never comes together) lets every fetch after it
/// go at once, so the counts, not a hang, fail the test.
struct Rounds<'a> {
    store: &'a Store,
    /// The level of each manifest.
    level: HashMap<TreeDigest, usize>,
    /// How many of each level are to be in flight together.
    together: Vec<usize>,
    /// How many of each level have begun.
    begun: Vec<AtomicUsize>,
    in_flight: AtomicUsize,
    most: AtomicUsize,
    rounds: AtomicUsize,
    gave_up: AtomicBool,
}

impl<'a> Rounds<'a> {
    /// Over the tree `root` of `store`, whose every manifest is listed at one level only (as in
    /// [`Store::binary`]), with `width` fetched at once.
    fn new(store: &'a Store, root: &TreeDigest, width: usize) -> Self {
        let mut level = HashMap::new();
        let mut sizes = Vec::new();
        let mut this = vec![*root];
        while !this.is_empty() {
            for digest in &this {
                level.insert(*digest, sizes.len());
            }
            sizes.push(this.len());
            this = this.iter().flat_map(|digest| store.dirs(digest)).collect();
        }
        Self {
            store,
            level,
            together: sizes.iter().map(|size| (*size).min(width)).collect(),
            begun: sizes.iter().map(|_| AtomicUsize::new(0)).collect(),
            in_flight: AtomicUsize::new(0),
            most: AtomicUsize::new(0),
            rounds: AtomicUsize::new(0),
            gave_up: AtomicBool::new(false),
        }
    }

    fn fetch(&self, digest: &TreeDigest) -> Result<Vec<u8>, String> {
        let now = self.in_flight.fetch_add(1, Ordering::SeqCst) + 1;
        if now == 1 {
            self.rounds.fetch_add(1, Ordering::SeqCst);
        }
        self.most.fetch_max(now, Ordering::SeqCst);
        let level = self.level[digest];
        self.begun[level].fetch_add(1, Ordering::SeqCst);
        let met = (0..LOOKS).any(|_| {
            std::thread::yield_now();
            self.gave_up.load(Ordering::SeqCst)
                || self.begun[level].load(Ordering::SeqCst) >= self.together[level]
        });
        if !met {
            self.gave_up.store(true, Ordering::SeqCst);
        }
        let got = self.store.get(digest);
        self.in_flight.fetch_sub(1, Ordering::SeqCst);
        got
    }
}

/// Asserts that a tree of 5 levels of distinct manifests, 1, 2, 4, 8 and 16 of them, takes 5
/// rounds of fetches, one for each level, where `flatten` takes one for each of its 31 manifests:
/// with every answer taking the same time, the attach takes about 5 answers' time, not 31. The 16
/// of the last level are fetched together, each manifest once, and the entries are those of
/// `flatten`.
#[test]
fn a_tree_costs_one_round_of_fetches_for_each_level() {
    let mut store = Store::default();
    let root = store.binary(5, "");
    let serial = store.serial(&root).unwrap();
    let rounds = Rounds::new(&store, &root, 1);
    assert_eq!(rounds.level.len(), 31);
    let _ = store.take_asked();
    assert_eq!(
        flatten(&root, |digest| rounds.fetch(digest)).unwrap(),
        serial
    );
    assert_eq!(rounds.rounds.load(Ordering::SeqCst), 31);
    let _ = store.take_asked();

    let rounds = Rounds::new(&store, &root, WIDTH);
    let entries = flatten_in_waves(&root, WIDTH, |digest| rounds.fetch(digest));
    assert_eq!(entries.unwrap(), serial);
    assert!(!rounds.gave_up.load(Ordering::SeqCst));
    assert_eq!(rounds.rounds.load(Ordering::SeqCst), 5);
    assert_eq!(rounds.most.load(Ordering::SeqCst), 16);
    assert_eq!(store.take_asked(), (31, 1));
}

/// Asserts that no more than `width` manifests of a level are fetched at once: a level of 8 at
/// the width 3 has 3 in flight at most, and comes together 3 at a time.
#[test]
fn a_level_is_fetched_by_at_most_width_at_once() {
    let mut store = Store::default();
    let root = store.binary(4, "");
    let rounds = Rounds::new(&store, &root, 3);
    let entries = flatten_in_waves(&root, 3, |digest| rounds.fetch(digest));
    assert_eq!(entries.unwrap(), store.serial(&root).unwrap());
    assert!(!rounds.gave_up.load(Ordering::SeqCst));
    assert_eq!(rounds.most.load(Ordering::SeqCst), 3);
}

/// Asserts that a tree with a manifest that cannot be had or read is refused as `flatten` refuses
/// it, root or child: absent, bytes that are not a manifest, bytes of another manifest, another
/// version. At the width 1 no more is fetched than `flatten` fetches: the root and the leaves up to
/// the one that fails.
#[test]
fn a_manifest_that_cannot_be_had_or_read_is_refused_as_flatten_refuses_it() {
    let mut store = Store::default();
    let absent = TreeDigest::of(b"absent");
    let not_json = store.put(r#"{"v":1,"children":["#.to_owned());
    let other = store.put(manifest(&[file("x", 1)]));
    let wrong = TreeDigest::of(b"wrong");
    let bytes = store.blobs[&other].clone();
    store.blobs.insert(wrong, bytes);
    let version = store.put(r#"{"v":2,"children":[]}"#.to_owned());
    let leaf = store.put(manifest(&[file("y", 1)]));
    let mut roots = vec![absent];
    for bad in [absent, not_json, wrong, version] {
        roots.push(store.put(manifest(&[
            dir("a", &leaf),
            dir("b", &other),
            dir("c", &bad),
            dir("d", &leaf),
        ])));
    }
    for root in roots {
        let refused = store.serial(&root).unwrap_err();
        let (fetched, _) = store.take_asked();
        assert_eq!(store.waves(&root, 1).unwrap_err(), refused);
        assert_eq!(store.take_asked().0, fetched, "{refused:?}");
        assert_eq!(store.waves(&root, WIDTH).unwrap_err(), refused);
        let _ = store.take_asked();
    }
}

/// Asserts that the depth limit is judged as `flatten` judges it: a chain of 4096 names is
/// accepted, one of 4097 refused and one of 4097 ending in an empty directory accepted; a chain of
/// 6000 is refused with 4097 manifests fetched, as `flatten` fetches; and a subtree met first at a
/// shallow place and again deeper is refused for the depth it has there.
#[test]
fn the_depth_limit_is_judged_as_flatten_judges_it() {
    let mut store = Store::default();
    let deepest = store.chain(4096, true);
    let entries = store.waves(&deepest, WIDTH).unwrap();
    assert_eq!(entries, store.serial(&deepest).unwrap());
    assert_eq!(entries.len(), 4097);
    let over = store.chain(4097, true);
    assert_eq!(store.waves(&over, WIDTH), Err(TreeError::TreeTooDeep));
    let empty_at_bottom = store.chain(4097, false);
    assert_eq!(store.waves(&empty_at_bottom, WIDTH).unwrap().len(), 4097);
    let far = store.chain(6000, true);
    let _ = store.take_asked();
    assert_eq!(store.waves(&far, WIDTH), Err(TreeError::TreeTooDeep));
    assert_eq!(store.take_asked().0, 4097);

    let tall = store.chain(4095, true);
    let beside = store.put(manifest(&[dir("x", &tall)]));
    let root = store.put(manifest(&[dir("a", &tall), dir("b", &beside)]));
    assert_eq!(store.serial(&root), Err(TreeError::TreeTooDeep));
    assert_eq!(store.waves(&root, WIDTH), Err(TreeError::TreeTooDeep));
}

/// One manifest of 4096 files.
fn full_leaf(store: &mut Store) -> TreeDigest {
    let names: Vec<String> = (0..4096).map(|i| format!("{i:05}")).collect();
    store.put(manifest(
        &names.iter().map(|n| file(n, 1)).collect::<Vec<_>>(),
    ))
}

/// Asserts that the entry limit is judged as `flatten` judges it: 4095 directories that are all
/// one leaf of 4096 files, 2^24 - 1 entries, are gathered from their two manifests; a file more
/// beside them, or the whole below one more directory, is refused.
#[test]
fn the_entry_limit_is_judged_as_flatten_judges_it() {
    let mut store = Store::default();
    let leaf = full_leaf(&mut store);
    let dirs: Vec<String> = (0..4095).map(|i| dir(&format!("{i:05}"), &leaf)).collect();
    let full = store.put(manifest(&dirs));
    let held = gather(&full, WIDTH, &|digest: &TreeDigest| store.get(digest)).unwrap();
    assert_eq!(held.len(), 2);
    let mut children = dirs.clone();
    children.push(file("zzzzz", 1));
    let beside = store.put(manifest(&children));
    let below = store.put(manifest(&[dir("a", &full)]));
    for root in [beside, below] {
        assert_eq!(store.waves(&root, WIDTH), Err(TreeError::TreeTooLarge));
    }
}

/// The text of a manifest of 65536 links, and the directory `next` if there is one.
fn wide_manifest(next: Option<&TreeDigest>) -> String {
    let mut children: Vec<String> = (0..65536).map(|i| link(&format!("{i:05}"), "")).collect();
    children.extend(next.map(|d| dir("zzzzz", d)));
    manifest(&children)
}

/// Asserts that the children of the manifests fetched are added up as each comes in, and the walk
/// stops when they pass the limit: a chain of 300 distinct manifests of 65537 children is over it
/// at the 256th, and no more is asked for, as `flatten` asks for no more.
#[test]
fn a_chain_of_wide_manifests_is_refused_as_the_entries_pass_the_limit() {
    let mut levels = vec![TreeDigest::of(wide_manifest(None).as_bytes())];
    for _ in 1..300 {
        let text = wide_manifest(levels.last());
        levels.push(TreeDigest::of(text.as_bytes()));
    }
    let asked = AtomicUsize::new(0);
    let fetch = |digest: &TreeDigest| -> Result<Vec<u8>, String> {
        asked.fetch_add(1, Ordering::SeqCst);
        let level = levels.iter().position(|d| d == digest).unwrap();
        let next = level.checked_sub(1).map(|n| &levels[n]);
        Ok(wide_manifest(next).into_bytes())
    };
    let root = levels[299];
    assert_eq!(
        flatten_in_waves(&root, WIDTH, fetch),
        Err(TreeError::TreeTooLarge)
    );
    assert_eq!(asked.load(Ordering::SeqCst), 256);
}

/// Asserts that a fetch that panics on a thread that helps fetch a level panics the flatten, as it
/// would on the caller's thread: the caller fetches the first manifest of the level and waits
/// until the helper has begun the second, which panics.
#[test]
#[should_panic(expected = "a fetch on a helper")]
fn a_fetch_that_panics_on_a_helper_panics_the_flatten() {
    let mut store = Store::default();
    let leaf = store.put(manifest(&[file("f", 1)]));
    let other = store.put(manifest(&[file("g", 1)]));
    let root = store.put(manifest(&[dir("a", &leaf), dir("b", &other)]));
    let begun = AtomicUsize::new(0);
    let fetch = |digest: &TreeDigest| -> Result<Vec<u8>, String> {
        if *digest != root {
            begun.fetch_add(1, Ordering::SeqCst);
            if std::thread::current().name() == Some(HELPER) {
                panic!("a fetch on a helper");
            }
            let _ = (0..LOOKS).any(|_| {
                std::thread::yield_now();
                begun.load(Ordering::SeqCst) == 2
            });
        }
        store.get(digest)
    };
    let _ = flatten_in_waves(&root, 2, fetch);
}
