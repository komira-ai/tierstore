//! The expansion of a tree into queued blobs: the order of the walk, the manifests it shares with an
//! attach, what it refuses by name, and the bounds of a call. The upstream is held at a gate where a
//! test needs the walk to be in the middle of a fetch, and the progress a test waits for is an
//! event of the cache (a counter, a place in the queue), never a time.

use std::future::Future;
use std::pin::Pin;
use std::task::{Context, Poll};

use super::super::metrics::{Turned, Walked, read};
use super::super::pack_testing::{Pack, distinct};
use super::super::prefetch::Prefetched;
use super::super::source::blob_path;
use super::super::testing::{Rig, digest, until};
use super::super::{CacheConfig, locked};
use super::{Bound, Limits, Refusal, TooManyTrees, Tree, read_file};
use crate::Op;
use crate::tree_manifest::{MAX_DEPTH, MAX_ENTRIES, TreeDigest, TreeError};

/// An object of the upstream named by its digest.
struct Stored {
    key: String,
    sha: String,
    len: u64,
}

/// The object `cas/l/<sha256>` of `len` bytes, put in the upstream; `seed` makes it differ from the
/// others.
async fn stored(rig: &Rig, seed: usize, len: usize) -> Stored {
    let body = distinct(seed, len);
    let sha = digest(&body);
    let key = format!("cas/l/{sha}");
    rig.put(&key, &body).await;
    Stored {
        key,
        sha,
        len: len as u64,
    }
}

impl Stored {
    /// The child of a manifest that is this file, called `name`.
    fn file(&self, name: &str) -> String {
        let (sha, len) = (&self.sha, self.len);
        format!(r#"{{"name":"{name}","kind":"file","sha256":"{sha}","size":{len}}}"#)
    }
}

/// A file called `name` of `size` bytes with the digest `sha`, found in `key` from `offset`.
fn file_in(name: &str, sha: &str, size: u64, key: &str, offset: u64) -> String {
    let source = format!(r#"{{"key":"{key}","offset":{offset}}}"#);
    format!(r#"{{"name":"{name}","kind":"file","sha256":"{sha}","size":{size},"source":{source}}}"#)
}

/// The child of a manifest that is the directory whose manifest is `tree`.
fn dir(name: &str, tree: &TreeDigest) -> String {
    format!(r#"{{"name":"{name}","kind":"dir","sha256":"{tree}","size":1}}"#)
}

/// The child of a manifest that is a link.
fn link(name: &str) -> String {
    format!(r#"{{"name":"{name}","kind":"symlink","target":"elsewhere"}}"#)
}

/// A file of no bytes, whose digest is that of nothing.
fn empty(name: &str) -> String {
    let sha = digest(b"");
    format!(r#"{{"name":"{name}","kind":"file","sha256":"{sha}","size":0}}"#)
}

/// Puts the manifest text `text` in the upstream as the object `cas/l/<digest of text>`.
async fn put_text(rig: &Rig, text: &str) -> TreeDigest {
    let tree = TreeDigest::of(text.as_bytes());
    rig.put(&key_of(&tree), text.as_bytes()).await;
    tree
}

/// Puts the manifest whose children are `children` (sorted by name) in the upstream.
async fn manifest(rig: &Rig, children: &[String]) -> TreeDigest {
    let text = format!(r#"{{"v":1,"children":[{}]}}"#, children.join(","));
    put_text(rig, &text).await
}

/// The key of the object that is the manifest `tree`.
fn key_of(tree: &TreeDigest) -> String {
    format!("cas/l/{tree}")
}

/// A cache with one prefetch worker and the settings changed by `change`.
fn rig_with(name: &str, change: impl FnOnce(&mut CacheConfig)) -> Rig {
    Rig::with(name, |config| {
        config.prefetch.concurrency = 1;
        change(config);
    })
}

/// A cache with one prefetch worker.
fn rig(name: &str) -> Rig {
    rig_with(name, |_| {})
}

/// Polls `future` once, with a waker that does nothing: the test looks at what the cache did, and
/// polls again when it has.
fn step<F: Future>(future: &mut Pin<Box<F>>) -> Poll<F::Output> {
    let mut context = Context::from_waker(futures::task::noop_waker_ref());
    future.as_mut().poll(&mut context)
}

/// The names of the blobs that wait for a worker.
fn waiting(rig: &Rig) -> Vec<String> {
    locked(&rig.cache.inner.prefetch.queue).order()
}

/// The trees of calls that were expanded, refused and cut, and the blobs queued.
fn tally(rig: &Rig) -> (u64, u64, u64, u64) {
    let counters = rig.cache.counters();
    (
        counters.trees(Walked::Expanded),
        counters.trees(Walked::Refused),
        counters.trees(Walked::Cut),
        counters.blobs(Prefetched::Queued),
    )
}

/// Waits until `count` fills of the workers have left their blob in the cache.
async fn filled(rig: &Rig, count: u64) {
    let counters = rig.cache.counters();
    until("the workers have filled the blobs", || {
        read(&counters.prefetch_fills) == count
    })
    .await;
}

/// The refusal that `tree` ended with, as the text the call records for it.
fn refused(ended: &Tree) -> String {
    match ended {
        Tree::Refused(refusal) => refusal.to_string(),
        other => panic!("the tree was not refused: {other:?}"),
    }
}

/// Asserts that a tree expands to every blob of its manifests in the order the manifests list them,
/// a directory's blobs where the directory is listed: with the only worker held at the first blob,
/// the others wait in the queue in that order, and when it is let go every blob is in the cache. It
/// would catch a walk that queues in the order it fetched, or that skips a directory.
#[tokio::test]
async fn tree_expands_to_every_blob_of_its_manifests_in_manifest_order() {
    let rig = rig("texpand-order");
    let blobs = [
        stored(&rig, 1, 100).await,
        stored(&rig, 2, 200).await,
        stored(&rig, 3, 300).await,
        stored(&rig, 4, 400).await,
    ];
    let below_a = manifest(&rig, &[blobs[0].file("x"), blobs[1].file("y")]).await;
    let below_c = manifest(&rig, &[blobs[3].file("z")]).await;
    let files = [dir("a", &below_a), blobs[2].file("b"), dir("c", &below_c)];
    let root = manifest(&rig, &files).await;
    let gate = rig.tamper.hold_key(&blobs[0].key);

    let ended = rig.cache.expand_trees(&[root]).await.unwrap();
    assert_eq!(ended, [Tree::Expanded]);
    until("the worker is at the first blob", || {
        rig.calls_on(Op::Get, &blobs[0].key) == 1
    })
    .await;
    let rest: Vec<String> = blobs[1..].iter().map(|b| b.sha.clone()).collect();
    assert_eq!(waiting(&rig), rest);
    assert_eq!(tally(&rig), (1, 0, 0, 4));

    gate.add_permits(64);
    filled(&rig, 4).await;
    for blob in &blobs {
        assert!(rig.has_file(&blob.key), "{}", blob.key);
    }
    assert_eq!(rig.cache.held().1, 7);
}

/// Asserts that a directory listed at two places is read once and walked once: the upstream is asked
/// for its manifest once, and its blob is queued once and never joined, since the walk does not
/// reach it twice. It would catch a walk that goes through a shared directory at each place.
#[tokio::test]
async fn shared_manifest_is_fetched_once() {
    let rig = rig("texpand-shared");
    let blob = stored(&rig, 1, 500).await;
    let shared = manifest(&rig, &[blob.file("f")]).await;
    let root = manifest(&rig, &[dir("a", &shared), dir("b", &shared)]).await;

    let ended = rig.cache.expand_trees(&[root]).await.unwrap();
    assert_eq!(ended, [Tree::Expanded]);
    filled(&rig, 1).await;
    assert_eq!(rig.calls_on(Op::Get, &key_of(&shared)), 1);
    assert_eq!(rig.calls_on(Op::Get, &blob.key), 1);
    assert_eq!(tally(&rig), (1, 0, 0, 1));
    assert_eq!(rig.cache.counters().blobs(Prefetched::Joined), 0);
}

/// Asserts that a file of no bytes, a link and a directory with nothing in it queue nothing and ask
/// the upstream for nothing but their manifests. It would catch an empty file handed to the queue,
/// which no fill can make.
#[tokio::test]
async fn empty_files_and_symlinks_queue_nothing() {
    let rig = rig("texpand-empty");
    let nothing = manifest(&rig, &[]).await;
    let root = manifest(&rig, &[dir("d", &nothing), empty("e"), link("l")]).await;

    let ended = rig.cache.expand_trees(&[root]).await.unwrap();
    assert_eq!(ended, [Tree::Expanded]);
    assert_eq!(tally(&rig), (1, 0, 0, 0));
    assert!(waiting(&rig).is_empty());
    assert_eq!(rig.calls(Op::Get), 2);
    let counters = rig.cache.counters();
    for outcome in [Prefetched::Present, Prefetched::Joined, Prefetched::Busy] {
        assert_eq!(counters.blobs(outcome), 0);
    }
}

/// Asserts what a manifest the library refuses, and one the cache cannot fetch, ends a tree with,
/// each by its exact message, and that the tree after them is expanded: an unknown version, an
/// unknown kind, bytes that do not hash to the digest they were asked under, a manifest larger than
/// a fill may be (64 MiB), one the upstream does not have, and a directory below the root that is
/// refused. It would catch a refusal that ends the call.
#[tokio::test]
async fn refused_manifest_stops_only_its_own_tree() {
    let rig = rig("texpand-refused");
    let blob = stored(&rig, 1, 100).await;
    let version = put_text(&rig, r#"{"v":2,"children":[]}"#).await;
    let kind = r#"{"v":1,"children":[{"name":"a","kind":"fifo","sha256":"x","size":1}]}"#;
    let kind = put_text(&rig, kind).await;
    let wrong = TreeDigest::of(b"what it is called");
    rig.put(&key_of(&wrong), b"what it is").await;
    let large = TreeDigest::of(b"a manifest of more than 64 MiB");
    let body = vec![b'x'; (64 << 20) + 1];
    rig.put(&key_of(&large), &body).await;
    let missing = TreeDigest::of(b"nothing has this digest");
    let bad_below = manifest(&rig, &[dir("a", &version)]).await;
    let good = manifest(&rig, &[blob.file("f")]).await;

    let trees = [version, kind, wrong, large, missing, bad_below, good];
    let ended = rig.cache.expand_trees(&trees).await.unwrap();
    let texts: Vec<String> = ended[..6].iter().map(refused).collect();
    assert_eq!(
        texts,
        [
            "unsupported manifest version".to_owned(),
            "unsupported entry kind".to_owned(),
            format!("cannot fetch manifest {wrong}: mismatch"),
            format!("cannot fetch manifest {large}: too large"),
            format!("cannot fetch manifest {missing}: upstream 404"),
            "unsupported manifest version".to_owned(),
        ]
    );
    assert_eq!(ended[6], Tree::Expanded);
    filled(&rig, 1).await;
    assert_eq!(tally(&rig), (1, 6, 0, 1));
    assert!(rig.has_file(&blob.key));
}

/// Asserts that a manifest that cannot be read from the cache's directory after its fill is refused
/// with the kind of the error of the disk, and that a cache that is read-only refuses every tree: it
/// fills nothing. It would catch a read error that is taken for an empty manifest.
#[tokio::test]
async fn a_manifest_the_disk_cannot_give_or_a_read_only_cache_refuses_its_tree() {
    let rig = rig("texpand-disk");
    let tree = TreeDigest::of(b"a tree");
    let gone = rig.dir.path().join("nowhere");
    let why = read_file(&gone, &tree).await.unwrap_err();
    let text = format!("cannot read manifest {tree}: entity not found");
    assert_eq!(why.to_string(), text);

    let holder = rig.locked();
    let reader = rig.locked();
    let root = manifest(&rig, &[]).await;
    let ended = reader.expand_trees(&[root]).await.unwrap();
    let text = format!("cannot fetch manifest {root}: read-only");
    assert_eq!(refused(&ended[0]), text);
    drop(holder);
}

/// Asserts that blobs are queued as the walk meets them, not after it: with the manifest of the
/// second directory held at the upstream, the call has not answered, and the blob of the first
/// directory is queued and filled; when the manifest comes, the second directory's blob follows. It
/// would catch a walk that collects the blobs of the tree and queues them at its end.
#[tokio::test]
async fn expansion_queues_blobs_while_the_walk_goes_on() {
    let rig = rig("texpand-progress");
    let (first, second) = (stored(&rig, 1, 100).await, stored(&rig, 2, 200).await);
    let below_a = manifest(&rig, &[first.file("f")]).await;
    let below_b = manifest(&rig, &[second.file("f")]).await;
    let root = manifest(&rig, &[dir("a", &below_a), dir("b", &below_b)]).await;
    let gate = rig.tamper.hold_key(&key_of(&below_b));

    let trees = [root];
    let mut walk = Box::pin(rig.cache.expand_trees(&trees));
    until(
        "the first blob is queued, the walk at the second manifest",
        || {
            assert!(step(&mut walk).is_pending());
            tally(&rig).3 == 1
        },
    )
    .await;
    filled(&rig, 1).await;
    assert!(rig.has_file(&first.key));
    assert!(!rig.has_file(&second.key));
    assert_eq!(tally(&rig), (0, 0, 0, 1));

    gate.add_permits(64);
    assert_eq!(walk.await.unwrap(), [Tree::Expanded]);
    filled(&rig, 2).await;
    assert!(rig.has_file(&second.key));
}

/// Polls `first` until it is at the upstream for `key`, then `second` until it has joined that
/// fetch, and opens the `gate`: neither is allowed to finish before.
async fn joined_fetch<A: Future, B: Future>(
    rig: &Rig,
    key: &str,
    (first, second): (&mut Pin<Box<A>>, &mut Pin<Box<B>>),
) {
    let gate = rig.tamper.hold_key(key);
    until("the second has joined the fetch of the first", || {
        let _ = step(first);
        rig.calls_on(Op::Get, key) == 1
            && step(second).is_pending()
            && rig.cache.counters().joins(false) == 1
    })
    .await;
    gate.add_permits(64);
}

/// Asserts that an attach that wants the manifest the expansion is fetching joins the fetch, and
/// that an expansion that wants the manifest an attach is fetching joins that: the upstream is read
/// once for the manifest either way, and the join is counted as a fill socket request's. It would
/// catch an expansion that fetches by a path of its own.
#[tokio::test]
async fn attach_of_a_tree_joins_the_expansion_manifest_fetch_and_the_reverse() {
    let rig = rig("texpand-join-forward");
    let root = manifest(&rig, &[]).await;
    let (key, hex) = (key_of(&root), root.to_string());
    let trees = [root];
    let mut expansion = Box::pin(rig.cache.expand_trees(&trees));
    let mut attach = Box::pin(rig.cache.fill_manifest(&hex));
    joined_fetch(&rig, &key, (&mut expansion, &mut attach)).await;
    assert_eq!(attach.await, Ok(()));
    assert_eq!(expansion.await.unwrap(), [Tree::Expanded]);
    assert_eq!(rig.calls_on(Op::Get, &key), 1);

    let rig = rig_with("texpand-join-reverse", |_| {});
    let root = manifest(&rig, &[]).await;
    let (key, hex) = (key_of(&root), root.to_string());
    let trees = [root];
    let mut attach = Box::pin(rig.cache.fill_manifest(&hex));
    let mut expansion = Box::pin(rig.cache.expand_trees(&trees));
    joined_fetch(&rig, &key, (&mut attach, &mut expansion)).await;
    assert_eq!(attach.await, Ok(()));
    assert_eq!(expansion.await.unwrap(), [Tree::Expanded]);
    assert_eq!(rig.calls_on(Op::Get, &key), 1);
}

/// Asserts that a tree sent again after it was expanded reads no manifest and fetches no blob again:
/// every manifest is in the cache and every blob is present, counted `present`. It would catch an
/// expansion that goes to the upstream for what the cache holds.
#[tokio::test]
async fn a_tree_sent_twice_reads_no_manifest_and_fetches_no_blob_again() {
    let rig = rig("texpand-twice");
    let (a, b) = (stored(&rig, 1, 100).await, stored(&rig, 2, 200).await);
    let below = manifest(&rig, &[b.file("f")]).await;
    let root = manifest(&rig, &[a.file("a"), dir("d", &below)]).await;

    assert_eq!(
        rig.cache.expand_trees(&[root]).await.unwrap(),
        [Tree::Expanded]
    );
    filled(&rig, 2).await;
    let reads = rig.calls(Op::Get);
    assert_eq!(
        rig.cache.expand_trees(&[root]).await.unwrap(),
        [Tree::Expanded]
    );
    assert_eq!(rig.calls(Op::Get), reads);
    assert_eq!(tally(&rig), (2, 0, 0, 2));
    assert_eq!(rig.cache.counters().blobs(Prefetched::Present), 2);
}

/// Asserts that a call with more trees than `hydrate_max_trees` is refused whole, with nothing read
/// from the upstream, that one with exactly that many is expanded, and that a tree listed twice is
/// expanded once. It would catch a call that expands the first trees of too long a list.
#[tokio::test]
async fn max_trees_refuses_the_call_whole_and_a_duplicate_tree_is_expanded_once() {
    let rig = rig_with("texpand-trees", |config| config.prefetch.max_trees = 2);
    let blob = stored(&rig, 1, 100).await;
    let one = manifest(&rig, &[blob.file("f")]).await;
    let (two, three) = (
        manifest(&rig, &[]).await,
        manifest(&rig, &[link("l")]).await,
    );

    let why = rig
        .cache
        .expand_trees(&[one, two, three])
        .await
        .unwrap_err();
    assert_eq!(why, TooManyTrees { given: 3, max: 2 });
    assert_eq!(why.to_string(), "3 trees in one call, at most 2");
    assert_eq!(rig.calls(Op::Get), 0);
    assert_eq!(tally(&rig), (0, 0, 0, 0));

    let ended = rig.cache.expand_trees(&[one, one]).await.unwrap();
    assert_eq!(ended, [Tree::Expanded]);
    assert_eq!(tally(&rig), (1, 0, 0, 1));
    assert_eq!(rig.calls_on(Op::Get, &key_of(&one)), 1);
    let both = rig.cache.expand_trees(&[one, two]).await.unwrap();
    assert_eq!(both, [Tree::Expanded, Tree::Expanded]);
    filled(&rig, 1).await;
}

/// `count` stored objects of `len` bytes each, distinct from one another.
async fn several(rig: &Rig, count: usize, len: usize) -> Vec<Stored> {
    let mut all = Vec::new();
    for seed in 1..=count {
        all.push(stored(rig, seed, len).await);
    }
    all
}

/// The manifest of the files `blobs`, called `a`, `b`, and so on.
async fn flat(rig: &Rig, blobs: &[Stored]) -> TreeDigest {
    let names = ["a", "b", "c", "d", "e"];
    let files: Vec<String> = blobs.iter().zip(names).map(|(b, n)| b.file(n)).collect();
    manifest(rig, &files).await
}

/// Asserts that the walk stops at the file met once the call has queued `hydrate_max_blobs` blobs:
/// the blobs before it stay queued, the one that stopped the walk is counted `too_many_blobs`, the
/// rest of the tree is not read, and the trees after it are cut with no request for their
/// manifests. A tree of exactly that many blobs is whole. It would catch a bound applied per tree,
/// or one that lets a blob through.
#[tokio::test]
async fn max_blobs_after_expansion_drops_the_rest() {
    let rig = rig_with("texpand-blobs-exact", |config| {
        config.prefetch.max_blobs = 2
    });
    let blobs = several(&rig, 2, 100).await;
    let exact = flat(&rig, &blobs).await;
    assert_eq!(
        rig.cache.expand_trees(&[exact]).await.unwrap(),
        [Tree::Expanded]
    );
    filled(&rig, 2).await;
    assert_eq!(tally(&rig), (1, 0, 0, 2));
    let counters = rig.cache.counters();
    assert_eq!(counters.turned_count(Turned::TooManyBlobs), 0);

    let rig = rig_with("texpand-blobs-over", |config| config.prefetch.max_blobs = 2);
    let blobs = several(&rig, 4, 100).await;
    let over = flat(&rig, &blobs).await;
    let after = flat(&rig, &blobs[3..]).await;
    let ended = rig.cache.expand_trees(&[over, after]).await.unwrap();
    assert_eq!(ended, [Tree::Cut(Bound::Blobs), Tree::Cut(Bound::Blobs)]);
    filled(&rig, 2).await;
    assert_eq!(tally(&rig), (0, 0, 2, 2));
    let counters = rig.cache.counters();
    assert_eq!(counters.turned_count(Turned::TooManyBlobs), 1);
    assert_eq!(rig.calls_on(Op::Get, &key_of(&after)), 0);
    assert!(rig.has_file(&blobs[1].key));
    assert!(!rig.has_file(&blobs[2].key));
    assert_eq!(rig.calls_on(Op::Get, &blobs[2].key), 0);
}

/// Asserts that a blob the cache holds costs the call nothing: with `hydrate_max_blobs` of 1, a tree
/// whose first file is in the cache and whose second is not is whole, the first counted `present`.
/// It would catch a bound that counts what it did not queue.
#[tokio::test]
async fn blobs_the_cache_holds_cost_the_call_nothing() {
    let rig = rig_with("texpand-held", |config| config.prefetch.max_blobs = 1);
    let blobs = several(&rig, 2, 100).await;
    let first = flat(&rig, &blobs[..1]).await;
    let both = flat(&rig, &blobs).await;
    assert_eq!(
        rig.cache.expand_trees(&[first]).await.unwrap(),
        [Tree::Expanded]
    );
    filled(&rig, 1).await;

    assert_eq!(
        rig.cache.expand_trees(&[both]).await.unwrap(),
        [Tree::Expanded]
    );
    filled(&rig, 2).await;
    let counters = rig.cache.counters();
    assert_eq!(counters.blobs(Prefetched::Present), 1);
    assert_eq!(counters.blobs(Prefetched::Queued), 2);
}

/// Asserts that the walk stops at the blob that would take the call past `hydrate_max_bytes`, counted
/// `too_many_bytes`, with the blobs before it queued; that blobs adding up to exactly the bound are
/// whole; and that a blob larger than the bound by itself ends the walk with nothing queued. It would
/// catch a bound off by one, or one judged after the blob is queued.
#[tokio::test]
async fn max_bytes_after_expansion_drops_the_rest() {
    let change = |config: &mut CacheConfig| config.prefetch.max_bytes = 5000;
    let rig = rig_with("texpand-bytes-exact", change);
    let (a, b) = (stored(&rig, 1, 2000).await, stored(&rig, 2, 3000).await);
    let exact = manifest(&rig, &[a.file("a"), b.file("b")]).await;
    assert_eq!(
        rig.cache.expand_trees(&[exact]).await.unwrap(),
        [Tree::Expanded]
    );
    filled(&rig, 2).await;

    let rig = rig_with("texpand-bytes-over", change);
    let blobs = several(&rig, 3, 2000).await;
    let over = flat(&rig, &blobs).await;
    assert_eq!(
        rig.cache.expand_trees(&[over]).await.unwrap(),
        [Tree::Cut(Bound::Bytes)]
    );
    filled(&rig, 2).await;
    assert_eq!(tally(&rig), (0, 0, 1, 2));
    let counters = rig.cache.counters();
    assert_eq!(counters.turned_count(Turned::TooManyBytes), 1);
    assert!(!rig.has_file(&blobs[2].key));

    let rig = rig_with("texpand-bytes-large", change);
    let big = stored(&rig, 1, 6000).await;
    let large = flat(&rig, &[big]).await;
    let ended = rig.cache.expand_trees(&[large]).await.unwrap();
    assert_eq!(ended, [Tree::Cut(Bound::Bytes)]);
    assert_eq!(tally(&rig), (0, 0, 1, 0));
}

/// Asserts that a file whose `source.key` is under a prefix the proxy keeps for itself is not handed
/// to the queue and is counted `reserved_key`, and that the walk goes on to the files after it. It
/// would catch a manifest that makes the cache read the proxy's own keys.
#[tokio::test]
async fn reserved_source_key_in_a_manifest_is_refused() {
    let rig = rig("texpand-reserved");
    let blob = stored(&rig, 1, 100).await;
    let sha = digest(b"somewhere else");
    let files = [
        file_in("a", &sha, 10, "_journal/n/1", 0),
        file_in("b", &sha, 10, "_tiering/checkpoint/n", 5),
        file_in("c", &sha, 10, "_entries/x", 0),
        blob.file("d"),
    ];
    let root = manifest(&rig, &files).await;

    assert_eq!(
        rig.cache.expand_trees(&[root]).await.unwrap(),
        [Tree::Expanded]
    );
    filled(&rig, 1).await;
    let counters = rig.cache.counters();
    assert_eq!(counters.turned_count(Turned::ReservedKey), 3);
    assert_eq!(tally(&rig), (1, 0, 0, 1));
    assert_eq!(rig.calls(Op::Get), 2);
}

/// Asserts that a file with a `source` is filled from the range of the object it names: the blob
/// that lies in a pack is queued with its pack and offset, and the bytes in the cache are the
/// range. It would catch an expansion that queues every file as the whole object `cas/l/<sha256>`.
#[tokio::test]
async fn a_file_with_a_source_is_filled_from_its_range_of_the_pack() {
    let rig = rig("texpand-source");
    let pack = Pack::put(&rig, "packs/one", &[300, 400]).await;
    let files: Vec<String> = pack
        .blobs
        .iter()
        .zip(["a", "b"])
        .map(|((sha, offset, size), name)| file_in(name, sha, *size, &pack.key, *offset))
        .collect();
    let root = manifest(&rig, &files).await;

    assert_eq!(
        rig.cache.expand_trees(&[root]).await.unwrap(),
        [Tree::Expanded]
    );
    filled(&rig, 2).await;
    for (sha, offset, size) in &pack.blobs {
        let bytes = std::fs::read(blob_path(&rig.config.path, sha)).unwrap();
        let (from, to) = (*offset as usize, (*offset + *size) as usize);
        assert_eq!(bytes, pack.bytes()[from..to]);
    }
}

/// Asserts the limits of the library on the tree a walk reads, with small limits in place of the
/// library's: a tree whose distinct manifests list more entries than allowed, and one with a level
/// deeper than allowed, are refused by name, a manifest with no children is never too deep, the
/// edge itself is allowed, and the limits of the library are its own. It would catch a walk that
/// goes down a tree of any depth.
#[tokio::test]
async fn a_tree_over_the_entry_or_depth_limit_is_refused_by_name() {
    assert_eq!(
        (Limits::LIBRARY.entries, Limits::LIBRARY.depth),
        (MAX_ENTRIES, MAX_DEPTH)
    );
    let rig = rig("texpand-limits");
    let nothing = manifest(&rig, &[]).await;
    let leaf = manifest(&rig, &[link("a")]).await;
    let mid = manifest(&rig, &[dir("a", &leaf), link("b")]).await;
    let root = manifest(&rig, &[dir("a", &mid)]).await;
    let deep_empty = manifest(&rig, &[dir("a", &nothing)]).await;

    let with = |entries, depth| Limits { entries, depth };
    let walk = |limits: Limits, tree: TreeDigest| {
        let cache = &rig.cache;
        async move { cache.expand(limits, &[tree]).await.remove(0) }
    };
    assert_eq!(walk(with(100, 3), root).await, Tree::Expanded);
    let too_deep = Tree::Refused(Refusal::Manifest(TreeError::TreeTooDeep));
    assert_eq!(walk(with(100, 2), root).await, too_deep);
    assert_eq!(walk(with(4, 3), root).await, Tree::Expanded);
    let too_large = Tree::Refused(Refusal::Manifest(TreeError::TreeTooLarge));
    assert_eq!(walk(with(3, 3), root).await, too_large);
    assert_eq!(walk(with(1, 1), deep_empty).await, Tree::Expanded);
    assert_eq!(walk(with(1, 1), nothing).await, Tree::Expanded);
    assert_eq!(walk(with(0, 1), root).await, too_large);
    assert_eq!(walk(with(10, 0), nothing).await, Tree::Expanded);
    assert_eq!(walk(with(10, 0), leaf).await, too_deep);
}

/// Asserts the counters of the expansion after a scripted call, each by its exact value: with
/// `hydrate_max_blobs` of 2, a tree of two blobs and a key the proxy keeps, one the library refuses,
/// a tree whose first blob is past the bound, and a tree after it. It would catch a counter that
/// counts the wrong thing, or a family `/metricz` leaves out.
#[tokio::test]
async fn the_counters_of_the_expansion_after_a_scripted_run() {
    let rig = rig_with("texpand-counters", |config| config.prefetch.max_blobs = 2);
    let blobs = several(&rig, 4, 100).await;
    let sha = digest(b"somewhere else");
    let files = [
        blobs[0].file("a"),
        file_in("b", &sha, 10, "_journal/x", 0),
        blobs[1].file("c"),
    ];
    let ok = manifest(&rig, &files).await;
    let bad = put_text(&rig, r#"{"v":2,"children":[]}"#).await;
    let past = flat(&rig, &blobs[2..3]).await;
    let last = flat(&rig, &blobs[3..]).await;

    let ended = rig
        .cache
        .expand_trees(&[ok, bad, past, last])
        .await
        .unwrap();
    assert_eq!(ended.len(), 4);
    filled(&rig, 2).await;
    assert_eq!(tally(&rig), (1, 1, 2, 2));
    let counters = rig.cache.counters();
    assert_eq!(counters.turned_count(Turned::ReservedKey), 1);
    assert_eq!(counters.turned_count(Turned::TooManyBlobs), 1);
    assert_eq!(counters.turned_count(Turned::TooManyBytes), 0);
    let text = rig.cache.metrics();
    for line in [
        "tierstore_cache_hydrate_trees_total{outcome=\"expanded\"} 1",
        "tierstore_cache_hydrate_trees_total{outcome=\"refused\"} 1",
        "tierstore_cache_hydrate_trees_total{outcome=\"cut\"} 2",
        "tierstore_cache_hydrate_blobs_total{outcome=\"queued\"} 2",
        "tierstore_cache_hydrate_blobs_total{outcome=\"too_many_blobs\"} 1",
        "tierstore_cache_hydrate_blobs_total{outcome=\"too_many_bytes\"} 0",
        "tierstore_cache_hydrate_blobs_total{outcome=\"reserved_key\"} 1",
    ] {
        assert!(text.contains(&format!("{line}\n")), "{line}\n{text}");
    }
}
