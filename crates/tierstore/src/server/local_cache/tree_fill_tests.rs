//! The fill of a tree's new manifests by the cache: a chain of new manifests filled in one round
//! of the upstream for each level, with every manifest of a level in flight at once; a directory
//! the cache holds not gone into; the manifests left in the cache directory, flattened as the
//! mount flattens them; a missing manifest, root or child, failing with its digest; the limits
//! judged with no level below the one that passes them fetched; the time the request was given;
//! the words of the answer; and each fill timed on the cache's clock.
//!
//! The rounds are counted as events, not as time: every HEAD of the upstream (the first call of a
//! fill of a manifest) is held at a gate, and the test lets each level through once all of its
//! HEADs have arrived. A fill that asked for the manifests of a level one after another would never
//! have them all at the gate together, and the test would fail by its count of turns.

use std::time::Duration;

use super::super::object_key;
use super::{Limits, TreeFillError, TreeFilled};
use crate::server::local_cache::FillError;
use crate::server::local_cache::manifest_path;
use crate::server::local_cache::testing::{Rig, until};
use crate::tree_manifest::{
    MAX_DEPTH, MAX_ENTRIES, TreeDigest, TreeEntry, TreeError, flatten_in_waves,
};
use crate::tree_manifest_tests::{dir, file, manifest};
use crate::{Backend as _, Op};

/// More time than any test takes: what a fill that must not run out is given.
const LONG: Duration = Duration::from_secs(600);

/// The manifests of a tree, put in the upstream of a rig as the objects `cas/l/<sha256>`.
struct Upstream<'a> {
    rig: &'a Rig,
    texts: Vec<(TreeDigest, String)>,
}

impl<'a> Upstream<'a> {
    fn of(rig: &'a Rig) -> Self {
        Self {
            rig,
            texts: Vec::new(),
        }
    }

    /// The manifest of `children`, in the upstream; its digest.
    async fn put(&mut self, children: &[String]) -> TreeDigest {
        let text = manifest(children);
        let digest = TreeDigest::of(text.as_bytes());
        let key = object_key(&digest.to_string());
        if self.rig.tamper.memory.head(&key).await.is_err() {
            self.rig.put(&key, text.as_bytes()).await;
        }
        self.texts.push((digest, text));
        digest
    }

    /// A chain of `levels` manifests, each holding the next as the directory `d` and the file
    /// `tag`, the last the file alone: its root. Chains of other tags share no manifest.
    async fn chain(&mut self, levels: usize, tag: &str) -> TreeDigest {
        let mut below = self.put(&[file(tag, 1)]).await;
        for _ in 1..levels {
            below = self.put(&[dir("d", &below), file(tag, 1)]).await;
        }
        below
    }

    /// The entries of the tree `root` from these manifests, as the mount flattens them.
    fn flattened(&self, root: &TreeDigest) -> Vec<TreeEntry> {
        let fetch = |digest: &TreeDigest, _: usize| -> Result<Vec<u8>, String> {
            let found = self.texts.iter().find(|(d, _)| d == digest);
            found
                .map(|(_, text)| text.clone().into_bytes())
                .ok_or_else(|| format!("no manifest {digest}"))
        };
        flatten_in_waves(root, 16, fetch).unwrap()
    }
}

/// The entries of the tree `root` from the cache directory alone, as the mount reads it there.
fn from_the_cache(rig: &Rig, root: &TreeDigest) -> Vec<TreeEntry> {
    let fetch = |digest: &TreeDigest, _: usize| -> Result<Vec<u8>, String> {
        std::fs::read(manifest_path(&rig.config.path, &digest.to_string()))
            .map_err(|why| why.to_string())
    };
    flatten_in_waves(root, 16, fetch).unwrap()
}

/// The HEADs of the upstream on the object of `digest`.
fn heads_of(rig: &Rig, digest: &TreeDigest) -> u64 {
    rig.calls_on(Op::Head, &object_key(&digest.to_string()))
}

/// Lets the levels of a fill through the gate of the HEADs one at a time, each once all its HEADs
/// `widths` says it has have arrived together; then nothing more arrives.
async fn let_through(rig: &Rig, gate: &tokio::sync::Semaphore, widths: &[u64]) {
    let mut arrived = 0;
    for (level, width) in widths.iter().enumerate() {
        arrived += width;
        until(
            &format!("the {width} HEADs of level {level} at the gate"),
            || rig.tamper.heading() == arrived,
        )
        .await;
        gate.add_permits(usize::try_from(*width).unwrap());
    }
}

/// The text of the node's own fill histogram in the cache's `/metricz`, its sums and counts.
fn node_fills(rig: &Rig) -> Vec<String> {
    rig.cache
        .metrics()
        .lines()
        .filter(|line| {
            line.starts_with("tierstore_cache_manifest_fill_seconds_sum")
                || line.starts_with("tierstore_cache_manifest_fill_seconds_count")
        })
        .map(str::to_owned)
        .collect()
}

/// Asserts that a chain of six new manifests is filled in six rounds of the upstream, one manifest
/// in each, each round let through only once it has come and the next asked for only after it:
/// the answer counts one manifest at each level and takes the milliseconds the cache's clock moved
/// while it ran; every manifest is then in the cache directory, which flattens to the entries the
/// same manifests flatten to; each manifest was asked of the upstream once; and each fill is timed
/// as a fill for a tree.
#[tokio::test]
async fn a_chain_of_new_manifests_is_filled_in_one_round_of_the_upstream_for_each_level() {
    let rig = Rig::new("ttree-fill-chain");
    let mut upstream = Upstream::of(&rig);
    let root = upstream.chain(6, "f").await;
    let gate = rig.tamper.hold_heads();
    let cache = rig.cache.clone();
    let filling = tokio::spawn(async move { cache.fill_tree(&root, LONG).await });
    let_through(&rig, &gate, &[1, 1, 1, 1, 1]).await;
    until("the last level at the gate", || rig.tamper.heading() == 6).await;
    rig.clock.advance(7);
    gate.add_permits(1);
    let filled = filling.await.unwrap().unwrap();
    assert_eq!(
        filled,
        TreeFilled {
            ms: 7,
            levels: vec![1; 6],
        }
    );
    assert_eq!(from_the_cache(&rig, &root), upstream.flattened(&root));
    for (digest, _) in &upstream.texts {
        assert_eq!(heads_of(&rig, digest), 1);
    }
    assert_eq!(rig.calls(Op::Head), 6);
    assert_eq!(
        node_fills(&rig),
        [
            "tierstore_cache_manifest_fill_seconds_sum{via=\"fill\"} 0",
            "tierstore_cache_manifest_fill_seconds_count{via=\"fill\"} 0",
            "tierstore_cache_manifest_fill_seconds_sum{via=\"tree\"} 0.007",
            "tierstore_cache_manifest_fill_seconds_count{via=\"tree\"} 6",
        ]
    );
}

/// Asserts that the manifests of a level are all in flight at once, and the level below waits for
/// them: in a tree of 1, 2, 4 and 8 distinct new manifests, the 2 of the second level are at the
/// gate together before either is let through, then the 4, then the 8, which is 4 rounds for 15
/// manifests.
#[tokio::test]
async fn the_new_manifests_of_a_level_are_filled_at_once() {
    let rig = Rig::new("ttree-fill-wide");
    let mut upstream = Upstream::of(&rig);
    let mut level = Vec::new();
    for leaf in 0..8 {
        level.push(upstream.put(&[file(&format!("leaf{leaf}"), 1)]).await);
    }
    while level.len() > 1 {
        let mut above = Vec::new();
        for pair in level.chunks(2) {
            above.push(
                upstream
                    .put(&[dir("a", &pair[0]), dir("b", &pair[1])])
                    .await,
            );
        }
        level = above;
    }
    let root = level[0];
    let gate = rig.tamper.hold_heads();
    let cache = rig.cache.clone();
    let filling = tokio::spawn(async move { cache.fill_tree(&root, LONG).await });
    let_through(&rig, &gate, &[1, 2, 4, 8]).await;
    let filled = filling.await.unwrap().unwrap();
    assert_eq!(filled.levels, [1, 2, 4, 8]);
    assert_eq!(from_the_cache(&rig, &root), upstream.flattened(&root));
}

/// Asserts that a directory whose manifest the cache holds is not gone into, and a root the cache
/// holds is read but not counted as filled: a new root over an old subtree of three manifests and a
/// new chain of two asks the upstream for the root and the chain only; asked again, the tree has
/// nothing new below its root, which is read and counted as no fill.
#[tokio::test]
async fn a_directory_the_cache_holds_is_not_gone_into() {
    let rig = Rig::new("ttree-fill-held");
    let mut upstream = Upstream::of(&rig);
    let old_leaf = upstream.put(&[file("x", 1)]).await;
    let old_mid = upstream.put(&[dir("leaf", &old_leaf)]).await;
    let old = upstream.put(&[dir("mid", &old_mid)]).await;
    for digest in [old_leaf, old_mid, old] {
        rig.cache.fill_manifest(&digest.to_string()).await.unwrap();
    }
    let new = upstream.chain(2, "f").await;
    let root = upstream.put(&[dir("new", &new), dir("old", &old)]).await;
    let filled = rig.cache.fill_tree(&root, LONG).await.unwrap();
    assert_eq!(filled.levels, [1, 1, 1]);
    for digest in [old_leaf, old_mid, old] {
        assert_eq!(heads_of(&rig, &digest), 1, "filled before, and not again");
    }
    assert_eq!(rig.calls(Op::Head), 6);
    assert_eq!(from_the_cache(&rig, &root), upstream.flattened(&root));

    let again = rig.cache.fill_tree(&root, LONG).await.unwrap();
    assert_eq!(again.levels, [0, 0]);
    assert_eq!(rig.calls(Op::Head), 6);
}

/// Asserts that a directory the tree holds in two places is filled and counted once: one new
/// manifest under two names of the root, and again one level further down, asked of the upstream
/// once and counted at the level it was first met.
#[tokio::test]
async fn a_directory_met_twice_is_filled_once() {
    let rig = Rig::new("ttree-fill-twice");
    let mut upstream = Upstream::of(&rig);
    let shared = upstream.put(&[file("x", 1)]).await;
    let mid = upstream.put(&[dir("again", &shared)]).await;
    let root = upstream
        .put(&[dir("a", &shared), dir("b", &shared), dir("m", &mid)])
        .await;
    let filled = rig.cache.fill_tree(&root, LONG).await.unwrap();
    assert_eq!(filled.levels, [1, 2]);
    assert_eq!(heads_of(&rig, &shared), 1);
    assert_eq!(rig.calls(Op::Head), 3);
    assert_eq!(from_the_cache(&rig, &root), upstream.flattened(&root));
}

/// Asserts that a manifest the upstream does not have fails the fill with its digest, in the
/// words of `fill <sha256>`: a missing root, and a missing child below a root that is filled.
#[tokio::test]
async fn a_missing_manifest_fails_the_fill_with_its_digest() {
    let rig = Rig::new("ttree-fill-missing");
    let mut upstream = Upstream::of(&rig);
    let absent = TreeDigest::of(b"no such manifest");
    let failed = rig.cache.fill_tree(&absent, LONG).await.unwrap_err();
    assert_eq!(
        failed,
        TreeFillError::Fill {
            digest: absent,
            error: FillError::Upstream(404),
        }
    );
    assert_eq!(failed.to_string(), format!("upstream 404 at {absent}"));

    let root = upstream.put(&[dir("gone", &absent)]).await;
    let failed = rig.cache.fill_tree(&root, LONG).await.unwrap_err();
    assert_eq!(failed.to_string(), format!("upstream 404 at {absent}"));
    assert!(manifest_path(&rig.config.path, &root.to_string()).exists());
}

/// Asserts that a filled object that is not a manifest fails the fill as a read of it, with its
/// digest: bytes that are their digest's (so the fill keeps them) but are not JSON.
#[tokio::test]
async fn a_filled_object_that_is_not_a_manifest_fails_the_fill_as_unreadable() {
    let rig = Rig::new("ttree-fill-unreadable");
    let body = b"not a manifest";
    let digest = TreeDigest::of(body);
    rig.put(&object_key(&digest.to_string()), body).await;
    let failed = rig.cache.fill_tree(&digest, LONG).await.unwrap_err();
    assert!(
        matches!(&failed, TreeFillError::Read { digest: read, .. } if *read == digest),
        "{failed:?}"
    );
    assert!(
        failed
            .to_string()
            .starts_with(&format!("cannot read {digest}: malformed manifest: ")),
        "{failed}"
    );
}

/// Asserts that the children of the manifests filled are counted as each comes in, and a tree over
/// the entry limit is refused with no level below fetched: under a limit of 3 entries, a root of 2
/// directories whose first holds 2 more passes it at that directory, and neither of those 2 is
/// asked of the upstream.
#[tokio::test]
async fn a_tree_over_the_entry_limit_is_refused_with_no_level_below_fetched() {
    let rig = Rig::new("ttree-fill-entries");
    let mut upstream = Upstream::of(&rig);
    let deep: Vec<TreeDigest> = vec![
        upstream.put(&[file("c", 1)]).await,
        upstream.put(&[file("d", 1)]).await,
    ];
    let first = upstream
        .put(&[dir("c", &deep[0]), dir("d", &deep[1])])
        .await;
    let second = upstream.put(&[file("e", 1)]).await;
    let root = upstream.put(&[dir("a", &first), dir("b", &second)]).await;
    let limits = Limits {
        entries: 3,
        depth: MAX_DEPTH,
    };
    let refused = rig.cache.fill_tree_under(&root, LONG, limits).await;
    assert_eq!(refused, Err(TreeFillError::Limit(TreeError::TreeTooLarge)));
    assert_eq!(
        refused.unwrap_err().to_string(),
        TreeError::TreeTooLarge.to_string()
    );
    for digest in &deep {
        assert_eq!(
            heads_of(&rig, digest),
            0,
            "below the level that passed the limit"
        );
    }
}

/// Asserts that a manifest with children deeper than the depth limit is refused with no level
/// below fetched: under a limit of 2, a chain of 5 is refused at its third manifest, whose children
/// are at depth 3, and the two below it are not asked of the upstream; a chain of 2 passes.
#[tokio::test]
async fn a_tree_over_the_depth_limit_is_refused_with_no_level_below_fetched() {
    let rig = Rig::new("ttree-fill-depth");
    let mut upstream = Upstream::of(&rig);
    let limits = Limits {
        entries: MAX_ENTRIES,
        depth: 2,
    };
    let short = upstream.chain(2, "short").await;
    let passed = rig.cache.fill_tree_under(&short, LONG, limits).await;
    assert_eq!(passed.unwrap().levels, [1, 1]);
    let root = upstream.chain(5, "long").await;
    let refused = rig.cache.fill_tree_under(&root, LONG, limits).await;
    assert_eq!(refused, Err(TreeFillError::Limit(TreeError::TreeTooDeep)));
    let chain: Vec<TreeDigest> = upstream.texts[2..].iter().map(|(d, _)| *d).collect();
    // The chain of 5 was put from its bottom: its first two are below the one that was refused.
    for digest in &chain[..2] {
        assert_eq!(heads_of(&rig, digest), 0, "{digest}");
    }
    assert_eq!(heads_of(&rig, &chain[2]), 1);
}

/// Asserts that a fill that does not end in the time it was given is answered `spent`: with the
/// root's HEAD held, a fill given no time ends so, and the fill it started goes on and keeps the
/// root once the HEAD is let go.
#[tokio::test]
async fn a_fill_that_does_not_end_in_its_time_is_spent() {
    let rig = Rig::new("ttree-fill-spent");
    let mut upstream = Upstream::of(&rig);
    let root = upstream.chain(1, "f").await;
    let gate = rig.tamper.hold_heads();
    let spent = rig.cache.fill_tree(&root, Duration::ZERO).await;
    assert_eq!(spent, Err(TreeFillError::Spent));
    assert_eq!(spent.unwrap_err().to_string(), "spent");
    until("the root's HEAD at the gate", || rig.tamper.heading() == 1).await;
    gate.add_permits(1);
    let path = manifest_path(&rig.config.path, &root.to_string());
    until("the root kept", || path.exists()).await;
}

/// Asserts that the answer of a fill reads back as written, with one level or many, and that every
/// other answer is not taken for one: the words of a failure, a missing or extra space, a number
/// with a sign or not decimal, `ok` alone.
#[test]
fn the_answer_of_a_fill_reads_back_and_no_other_answer_does() {
    let filled = TreeFilled {
        ms: 31,
        levels: vec![1, 0, 2],
    };
    assert_eq!(filled.answer(), "ok 31 1 0 2");
    assert_eq!(TreeFilled::parse(&filled.answer()), Some(filled));
    let alone = TreeFilled {
        ms: 0,
        levels: Vec::new(),
    };
    assert_eq!(alone.answer(), "ok 0");
    assert_eq!(TreeFilled::parse("ok 0"), Some(alone));
    for other in [
        "",
        "ok",
        "ok ",
        "ok 31 ",
        "ok  31",
        "ok -1",
        "ok +1 2",
        "ok 1 x",
        "ok 1.5",
        "upstream 404 at abc",
        "refused unknown request",
        "spent",
    ] {
        assert_eq!(TreeFilled::parse(other), None, "{other:?}");
    }
}
