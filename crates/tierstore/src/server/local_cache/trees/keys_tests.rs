//! The keys placements show to a listing: which buckets have a placement, and the keys of a
//! bucket in key order under a prefix, after a start and up to a limit.

use super::files::RECORDS;
use super::testing::{Bench, built, fs_at, s3_at, tree_path};
use super::{CreateRequest, PlacementId, ReadError, TreeKey};
use crate::TreeDigest;

const BUCKET: &str = "build-cas";
const HOUR: u64 = 3600 * 1000;

async fn placed(name: &str) -> (Bench, PlacementId) {
    let bench = Bench::new(name).await;
    let made = bench
        .store
        .create(bench.request(vec![s3_at("jobs/1/root/")]))
        .unwrap();
    (bench, made.placement)
}

fn blob(bench: &Bench, i: usize) -> (u64, TreeDigest) {
    let (sha, _, size) = &bench.pack.blobs[i];
    (*size, TreeDigest::from_hex(sha).unwrap())
}

fn key(key: &str, (size, sha256): (u64, TreeDigest), created_ms: u64) -> TreeKey {
    TreeKey {
        key: key.to_owned(),
        size,
        sha256,
        created_ms,
    }
}

/// The keys of the small tree of the fixture below `at`, in key order.
fn small(bench: &Bench, at: &str, created_ms: u64) -> Vec<TreeKey> {
    let empty = (0, TreeDigest::of(b""));
    vec![
        key(&format!("{at}bin/other"), blob(bench, 1), created_ms),
        key(&format!("{at}bin/tool"), blob(bench, 0), created_ms),
        key(&format!("{at}lib/deep/data"), blob(bench, 2), created_ms),
        key(&format!("{at}link"), empty, created_ms),
        key(&format!("{at}zero"), empty, created_ms),
    ]
}

fn names(keys: &[TreeKey]) -> Vec<&str> {
    keys.iter().map(|k| k.key.as_str()).collect()
}

fn list(
    bench: &Bench,
    prefix: &str,
    after: Option<&str>,
    limit: usize,
) -> Result<Vec<TreeKey>, ReadError> {
    bench.store.s3_keys(BUCKET, prefix, after, limit)
}

/// Asserts that a bucket has a placement only while a live s3 placement has a location in it: not
/// before any create, not for another bucket, not when the only placement is of the mount, and not
/// once the placement is released.
#[tokio::test]
async fn places_in_is_true_only_for_a_bucket_with_a_live_s3_placement() {
    let bench = Bench::new("keys-places").await;
    assert!(!bench.store.places_in(BUCKET));
    let made = bench
        .store
        .create(bench.request(vec![fs_at("mount/1")]))
        .unwrap();
    assert!(!bench.store.places_in(BUCKET));
    bench.store.release(&made.placement).unwrap();
    let s3 = bench
        .store
        .create(bench.request(vec![s3_at("jobs/1/root/")]))
        .unwrap();
    assert!(bench.store.places_in(BUCKET));
    assert!(!bench.store.places_in("other"));
    assert!(!bench.store.places_in("build-ca"));
    bench.store.release(&s3.placement).unwrap();
    assert!(!bench.store.places_in(BUCKET));
}

/// Asserts that a bucket stops having a placement when the lease of its only placement ends on the
/// injected clock, before anything is swept.
#[tokio::test]
async fn places_in_is_false_once_the_lease_has_ended() {
    let (bench, _id) = placed("keys-places-lease").await;
    assert!(bench.store.places_in(BUCKET));
    bench.rig.clock.advance(4 * HOUR - 1);
    assert!(bench.store.places_in(BUCKET));
    bench.rig.clock.advance(1);
    assert!(!bench.store.places_in(BUCKET));
}

/// Asserts that an empty prefix lists every file and symbolic link of the tree as a full key, in
/// key order, with the file's size and digest, size 0 and the digest of nothing for the link and
/// for the empty file, and the placement's creation time; and that the empty directory shows no key.
#[tokio::test]
async fn the_whole_tree_is_listed_in_key_order_with_size_digest_and_time() {
    let (bench, id) = placed("keys-all").await;
    let made = bench.store.placement(&id).unwrap().created_ms;
    let keys = list(&bench, "", None, 100).unwrap();
    assert_eq!(keys, small(&bench, "jobs/1/root/", made));
    assert_eq!(keys[3].size, 0);
    assert_eq!(keys[3].sha256, TreeDigest::of(b""));
    assert_eq!(keys[0].size, 2000);
    assert!(!names(&keys).iter().any(|k| k.contains("empty")));
}

/// Asserts that a listing of another bucket meets nothing, and that a store with no placement lists
/// nothing.
#[tokio::test]
async fn another_bucket_or_no_placement_lists_nothing() {
    let (bench, _id) = placed("keys-bucket").await;
    assert_eq!(bench.store.s3_keys("other", "", None, 100).unwrap(), vec![]);
    let bare = Bench::new("keys-bare").await;
    assert_eq!(list(&bare, "", None, 100).unwrap(), vec![]);
}

/// Asserts that a prefix inside the placement lists the keys below it (a directory, a partial name
/// that meets a directory and a file, a whole file name, a directory below a directory), that the
/// prefix of the placement without its slash lists everything, and that a prefix above the
/// placement lists all of it.
#[tokio::test]
async fn a_prefix_selects_the_keys_that_begin_with_it() {
    let (bench, id) = placed("keys-prefix").await;
    let made = bench.store.placement(&id).unwrap().created_ms;
    let all = small(&bench, "jobs/1/root/", made);
    let only = |from: usize, to: usize| all[from..to].to_vec();
    assert_eq!(
        list(&bench, "jobs/1/root/bin/", None, 100).unwrap(),
        only(0, 2)
    );
    assert_eq!(
        list(&bench, "jobs/1/root/bin", None, 100).unwrap(),
        only(0, 2)
    );
    assert_eq!(
        list(&bench, "jobs/1/root/bin/t", None, 100).unwrap(),
        only(1, 2)
    );
    assert_eq!(
        list(&bench, "jobs/1/root/li", None, 100).unwrap(),
        only(2, 4)
    );
    assert_eq!(
        list(&bench, "jobs/1/root/link", None, 100).unwrap(),
        only(3, 4)
    );
    assert_eq!(
        list(&bench, "jobs/1/root/lib/deep/", None, 100).unwrap(),
        only(2, 3)
    );
    assert_eq!(
        list(&bench, "jobs/1/root/z", None, 100).unwrap(),
        only(4, 5)
    );
    assert_eq!(list(&bench, "jobs/1/root", None, 100).unwrap(), all);
    assert_eq!(list(&bench, "jobs/1/", None, 100).unwrap(), all);
    assert_eq!(list(&bench, "jobs/", None, 100).unwrap(), all);
    assert_eq!(list(&bench, "j", None, 100).unwrap(), all);
}

/// Asserts that a prefix that does not meet the placement lists nothing: a sibling job, another
/// top directory, a name that only begins like the placement's, a missing directory inside it, and
/// the name of a file followed by a slash.
#[tokio::test]
async fn a_prefix_that_does_not_meet_the_placement_lists_nothing() {
    let (bench, _id) = placed("keys-miss").await;
    for prefix in [
        "jobs/2/",
        "other/",
        "jobs/1/rootx",
        "jobs/1/root/nothing",
        "jobs/1/root/bin/zzz",
        "jobs/1/root/link/",
        "jobs/1/root/empty",
        "z",
    ] {
        assert_eq!(list(&bench, prefix, None, 100).unwrap(), vec![], "{prefix}");
    }
}

/// Asserts that a listing after a key gives only the keys that sort after it, whether the start is
/// a key, a name inside a directory run, a directory with a slash, a point past every key or before
/// all of them, and that a start combines with a prefix.
#[tokio::test]
async fn after_skips_keys_up_to_and_including_the_start() {
    let (bench, id) = placed("keys-after").await;
    let made = bench.store.placement(&id).unwrap().created_ms;
    let all = small(&bench, "jobs/1/root/", made);
    let from = |n: usize| all[n..].to_vec();
    let after = |after: &str| list(&bench, "", Some(after), 100).unwrap();
    assert_eq!(after("jobs/1/root/bin/other"), from(1));
    assert_eq!(after("jobs/1/root/bin/tool"), from(2));
    assert_eq!(after("jobs/1/root/bin/p"), from(1));
    assert_eq!(after("jobs/1/root/bin/zzz"), from(2));
    assert_eq!(after("jobs/1/root/lib/deep/a"), from(2));
    assert_eq!(after("jobs/1/root/lib/deep/data"), from(3));
    assert_eq!(after("jobs/1/root/lib/deep/"), from(2));
    assert_eq!(after("jobs/1/root/lib/"), from(2));
    assert_eq!(after("jobs/1/root/lib"), from(2));
    assert_eq!(after("jobs/1/root/link"), from(4));
    assert_eq!(after("jobs/1/root/zero"), vec![]);
    assert_eq!(after("jobs/1/root/zzz"), vec![]);
    assert_eq!(after("jobs/2"), vec![]);
    assert_eq!(after("jobs/1"), all);
    assert_eq!(after("a"), all);
    assert_eq!(after(""), all);
    let within = list(
        &bench,
        "jobs/1/root/bin/",
        Some("jobs/1/root/bin/other"),
        100,
    )
    .unwrap();
    assert_eq!(within, all[1..2].to_vec());
    assert_eq!(
        list(
            &bench,
            "jobs/1/root/bin/",
            Some("jobs/1/root/bin/tool"),
            100
        )
        .unwrap(),
        vec![]
    );
}

/// Asserts that a limit gives exactly the first keys in order, that a limit of one gives the first
/// key, that a limit of the count or more gives all, that a limit of zero gives none, and that the
/// next listing can start after the last key of the one before, until the keys are over.
#[tokio::test]
async fn limit_truncates_to_the_first_keys_and_pages_chain() {
    let (bench, id) = placed("keys-limit").await;
    let made = bench.store.placement(&id).unwrap().created_ms;
    let all = small(&bench, "jobs/1/root/", made);
    assert_eq!(list(&bench, "", None, 3).unwrap(), all[..3].to_vec());
    assert_eq!(list(&bench, "", None, 1).unwrap(), all[..1].to_vec());
    assert_eq!(list(&bench, "", None, 5).unwrap(), all);
    assert_eq!(list(&bench, "", None, 6).unwrap(), all);
    assert_eq!(list(&bench, "", None, 0).unwrap(), vec![]);
    let mut seen = Vec::new();
    let mut after: Option<String> = None;
    loop {
        let page = list(&bench, "", after.as_deref(), 2).unwrap();
        let Some(last) = page.last() else { break };
        after = Some(last.key.clone());
        seen.extend(page);
    }
    assert_eq!(seen, all);
}

/// Asserts that keys come in the order of the keys and not of the names: the file `a-b` is before
/// the keys of the directory `a` (`-` is below `/`), those are before the file `a0` (`/` is below
/// `0`), for the whole listing, for a limit that cuts inside the directory, for a start inside it,
/// and for the prefixes `p/a`, `p/a/` and `p/a-`.
#[tokio::test]
async fn keys_are_in_key_order_not_name_order() {
    let bench = Bench::new("keys-order").await;
    let tree = built(vec![
        bench.member("a/x", 0),
        bench.member("a-b", 1),
        bench.member("a0", 2),
        bench.member("a/y/z", 0),
    ]);
    let made = bench
        .store
        .create(CreateRequest {
            built: tree,
            at: vec![s3_at("p/")],
            ttl: None,
            ready: false,
        })
        .unwrap();
    let at = bench.store.placement(&made.placement).unwrap().created_ms;
    let all = vec![
        key("p/a-b", blob(&bench, 1), at),
        key("p/a/x", blob(&bench, 0), at),
        key("p/a/y/z", blob(&bench, 0), at),
        key("p/a0", blob(&bench, 2), at),
    ];
    assert_eq!(list(&bench, "", None, 10).unwrap(), all);
    assert_eq!(list(&bench, "", None, 2).unwrap(), all[..2].to_vec());
    assert_eq!(list(&bench, "", None, 3).unwrap(), all[..3].to_vec());
    assert_eq!(
        list(&bench, "", Some("p/a-b"), 10).unwrap(),
        all[1..].to_vec()
    );
    assert_eq!(
        list(&bench, "", Some("p/a/x"), 10).unwrap(),
        all[2..].to_vec()
    );
    assert_eq!(
        list(&bench, "", Some("p/a/y"), 10).unwrap(),
        all[2..].to_vec()
    );
    assert_eq!(
        list(&bench, "", Some("p/a/z"), 10).unwrap(),
        all[3..].to_vec()
    );
    assert_eq!(
        list(&bench, "", Some("p/a/"), 10).unwrap(),
        all[1..].to_vec()
    );
    assert_eq!(list(&bench, "", Some("p/a0"), 10).unwrap(), vec![]);
    assert_eq!(list(&bench, "p/a", None, 10).unwrap(), all);
    assert_eq!(list(&bench, "p/a/", None, 10).unwrap(), all[1..3].to_vec());
    assert_eq!(list(&bench, "p/a-", None, 10).unwrap(), all[..1].to_vec());
    assert_eq!(list(&bench, "p/a0", None, 10).unwrap(), all[3..].to_vec());
}

/// Asserts that two placements of one tree at different prefixes list together under a prefix above
/// both, each key carrying the creation time of its own placement, that a prefix inside one lists
/// only that one, and that the limit and the start apply across both.
#[tokio::test]
async fn two_placements_of_one_tree_list_together_in_key_order() {
    let (bench, first) = placed("keys-two").await;
    bench.rig.clock.advance(1000);
    let second = bench
        .store
        .create(bench.request(vec![s3_at("jobs/2/root/")]))
        .unwrap()
        .placement;
    let one = bench.store.placement(&first).unwrap().created_ms;
    let two = bench.store.placement(&second).unwrap().created_ms;
    assert_ne!(one, two);
    let mut all = small(&bench, "jobs/1/root/", one);
    all.extend(small(&bench, "jobs/2/root/", two));
    assert_eq!(list(&bench, "jobs/", None, 100).unwrap(), all);
    assert_eq!(list(&bench, "", None, 100).unwrap(), all);
    assert_eq!(list(&bench, "jobs/", None, 7).unwrap(), all[..7].to_vec());
    assert_eq!(list(&bench, "jobs/", None, 5).unwrap(), all[..5].to_vec());
    assert_eq!(list(&bench, "jobs/", None, 6).unwrap(), all[..6].to_vec());
    assert_eq!(
        list(&bench, "jobs/2/", None, 100).unwrap(),
        all[5..].to_vec()
    );
    assert_eq!(
        list(&bench, "jobs/1/", None, 100).unwrap(),
        all[..5].to_vec()
    );
    assert_eq!(
        list(&bench, "jobs/", Some("jobs/1/root/zero"), 3).unwrap(),
        all[5..8].to_vec()
    );
    assert_eq!(
        list(&bench, "jobs/", Some("jobs/1/root/link"), 100).unwrap(),
        all[4..].to_vec()
    );
}

/// Asserts that a released placement shows no key, and that releasing one of two placements
/// leaves the keys of the other.
#[tokio::test]
async fn a_released_placement_shows_no_keys() {
    let (bench, first) = placed("keys-released").await;
    let second = bench
        .store
        .create(bench.request(vec![s3_at("jobs/2/root/")]))
        .unwrap()
        .placement;
    let both = list(&bench, "", None, 100).unwrap();
    assert_eq!(both.len(), 10);
    bench.store.release(&first).unwrap();
    let rest = list(&bench, "", None, 100).unwrap();
    assert_eq!(rest, both[5..].to_vec());
    bench.store.release(&second).unwrap();
    assert_eq!(list(&bench, "", None, 100).unwrap(), vec![]);
}

/// Asserts that a placement whose lease ended on the injected clock shows no keys, before any sweep.
#[tokio::test]
async fn a_placement_whose_lease_ended_shows_no_keys() {
    let (bench, _id) = placed("keys-lease").await;
    bench.rig.clock.advance(4 * HOUR - 1);
    assert_eq!(list(&bench, "", None, 100).unwrap().len(), 5);
    bench.rig.clock.advance(1);
    assert_eq!(list(&bench, "", None, 100).unwrap(), vec![]);
}

/// Asserts that a listing of a placement whose records file was cut, or is gone, answers `Corrupt`
/// and not a partial list, that it works again when the file is whole, and that a prefix that does
/// not meet the placement reads nothing and so does not fail.
#[tokio::test]
async fn damaged_records_answer_corrupt() {
    let (bench, _id) = placed("keys-damaged").await;
    let file = tree_path(&bench.rig, &bench.built().root).join(RECORDS);
    let good = std::fs::read(&file).unwrap();
    let mut cut = good.clone();
    cut.truncate(cut.len() - 4);
    std::fs::write(&file, &cut).unwrap();
    assert!(matches!(
        list(&bench, "", None, 100),
        Err(ReadError::Corrupt(_))
    ));
    std::fs::write(&file, &good).unwrap();
    assert_eq!(list(&bench, "", None, 100).unwrap().len(), 5);
    std::fs::remove_file(&file).unwrap();
    assert!(matches!(
        list(&bench, "", None, 100),
        Err(ReadError::Corrupt(_))
    ));
    assert_eq!(list(&bench, "jobs/2/", None, 100).unwrap(), vec![]);
}

/// Asserts that a directory of more children than one page of the store's listing (1000) is read
/// whole: all 1001 files of it are listed, in key order; a start at the last child of the first
/// page gives the one child of the second, and a start at that child gives none.
#[tokio::test]
async fn a_directory_of_more_than_a_page_of_children_is_listed_whole() {
    let bench = Bench::new("keys-many").await;
    let entries = (0..1001)
        .map(|i| bench.member(&format!("many/f{i:04}"), 0))
        .collect();
    let request = CreateRequest {
        built: built(entries),
        at: vec![s3_at("p/")],
        ttl: None,
        ready: false,
    };
    bench.store.create(request).unwrap();
    let keys: Vec<String> = bench
        .store
        .s3_keys(BUCKET, "", None, 5000)
        .unwrap()
        .into_iter()
        .map(|key| key.key)
        .collect();
    let expected: Vec<String> = (0..1001).map(|i| format!("p/many/f{i:04}")).collect();
    assert_eq!(keys, expected);
    let rest = bench
        .store
        .s3_keys(BUCKET, "", Some("p/many/f0999"), 5000)
        .unwrap();
    let rest: Vec<&str> = rest.iter().map(|key| key.key.as_str()).collect();
    assert_eq!(rest, vec!["p/many/f1000"]);
    let past = bench
        .store
        .s3_keys(BUCKET, "", Some("p/many/f1000"), 5000)
        .unwrap();
    assert_eq!(past, vec![]);
}
