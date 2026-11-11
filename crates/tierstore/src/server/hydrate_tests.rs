//! The hydrate and release calls: what each answers, what the cache does after the answer, and who
//! may call. The upstream of the cache is held at a gate where a test needs the expansion to be in
//! the middle of a fetch, and the progress a test waits for is a counter of the cache, never a time.

use bytes::Bytes;
use http_body_util::Full;
use hyper::{Method, Request, StatusCode};
use serde_json::{Value, json};

use super::{Call, asked, refused};
use crate::Op;
use crate::server::keys::AccessKey;
use crate::server::local_cache::expand::{HydrateError, TooManyTrees};
use crate::server::local_cache::holds::HoldError;
use crate::server::local_cache::pack_testing::distinct;
use crate::server::local_cache::testing::{Rig as CacheRig, digest, until};
use crate::server::local_cache::{CacheConfig, Mode};
use crate::server::presign::{PresignRequest, presign_url};
use crate::server::scoped::{Prefixes, ScopedKey};
use crate::server::testing::{KEY_ID, NOW, Reply, Rig, SECRET, texts};
use crate::tree_manifest::TreeDigest;

const READER: (&str, &str) = ("AKIDREADER", "reader-secret-0123456789");
const NARROW: (&str, &str) = ("narrow", "narrow-secret-0123456789");
const WIDE: (&str, &str) = ("wide", "wide-secret-0123456789");
const WRITER: (&str, &str) = ("writer", "writer-secret-0123456789");

/// A server in front of a cache, and the cache, whose upstream the test fills.
struct Fixture {
    cache: CacheRig,
    server: Rig,
}

/// An object of the upstream named by its digest.
struct Stored {
    key: String,
    sha: String,
    len: u64,
}

impl Stored {
    /// The child of a manifest that is this file, called `name`.
    fn file(&self, name: &str) -> String {
        let (sha, len) = (&self.sha, self.len);
        format!(r#"{{"name":"{name}","kind":"file","sha256":"{sha}","size":{len}}}"#)
    }
}

/// A scoped key with the id and secret `key`, reading under `read` and writing under `write`.
fn scoped(key: (&str, &str), read: &[&str], write: &[&str]) -> ScopedKey {
    let prefixes = Prefixes {
        read,
        write,
        ..Prefixes::default()
    };
    ScopedKey::new(key.0, key.1, &prefixes).unwrap()
}

/// A fixture whose cache has one prefetch worker and the settings changed by `change`, and whose
/// server has a read-only key and three scoped keys.
async fn fixture_with(name: &str, change: impl FnOnce(&mut CacheConfig)) -> Fixture {
    let cache = CacheRig::with(name, |config| {
        config.prefetch.concurrency = 1;
        change(config);
    });
    let held = cache.cache.clone();
    let server = Rig::with(1 << 20, move |config| {
        config.cache = Some(held);
        config.read_only_keys = vec![AccessKey::new(READER.0, READER.1)];
        config.scoped_keys = vec![
            scoped(NARROW, &["cas/l/"], &[]),
            scoped(WIDE, &["cas/f/"], &[]),
            scoped(WRITER, &[], &["w/"]),
        ];
    })
    .await;
    Fixture { cache, server }
}

async fn fixture(name: &str) -> Fixture {
    fixture_with(name, |_| {}).await
}

/// The object `cas/l/<sha256>` of `len` bytes, put in the upstream.
async fn stored(rig: &CacheRig, seed: usize, len: usize) -> Stored {
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

/// Puts the manifest whose children are `children` in the upstream.
async fn manifest(rig: &CacheRig, children: &[String]) -> TreeDigest {
    let text = format!(r#"{{"v":1,"children":[{}]}}"#, children.join(","));
    let tree = TreeDigest::of(text.as_bytes());
    rig.put(&key_of(&tree), text.as_bytes()).await;
    tree
}

/// The key of the object that is the manifest `tree`.
fn key_of(tree: &TreeDigest) -> String {
    format!("cas/l/{tree}")
}

/// The body of a hydrate call for the hold `hold` and `trees`.
fn hydrate_body(hold: &str, trees: &[TreeDigest]) -> String {
    let trees: Vec<String> = trees.iter().map(ToString::to_string).collect();
    json!({"hold": hold, "trees": trees}).to_string()
}

/// The JSON of an answer.
fn json_of(reply: &Reply) -> Value {
    serde_json::from_str(&reply.body).expect("a JSON body")
}

/// The status, the code and the message of an error answer.
fn error_of(reply: &Reply) -> (u16, String, String) {
    let message = texts(&reply.body, "Message").concat();
    (reply.status.as_u16(), reply.code(), message)
}

/// A bodyless `POST` of `{}` to `path`, with no `Authorization` header.
fn unsigned(path: &str, host: &str) -> Request<Full<Bytes>> {
    Request::builder()
        .method("POST")
        .uri(path)
        .header("host", host)
        .body(Full::new(Bytes::from_static(b"{}")))
        .unwrap()
}

impl Fixture {
    /// Sends a `POST` of `body` to the call `call`, signed with the full key.
    async fn post(&self, call: &str, body: &str) -> Reply {
        let path = format!("/_tierstore/v1/{call}");
        self.server.send("POST", &path, &[], body.as_bytes()).await
    }

    /// Sends the same, signed with `key`.
    async fn post_as(&self, key: (&str, &str), call: &str, body: &str) -> Reply {
        let path = format!("/_tierstore/v1/{call}");
        let body = body.as_bytes();
        self.server.send_as(key, "POST", &path, &[], body).await
    }

    /// The value of the sample `sample` of the cache's `/metricz`.
    fn metric(&self, sample: &str) -> u64 {
        let text = self.cache.cache.metrics();
        let line = text
            .lines()
            .find(|line| line.starts_with(&format!("{sample} ")))
            .unwrap_or_else(|| panic!("no sample {sample} in {text}"));
        line.rsplit(' ').next().unwrap().parse().unwrap()
    }

    /// Waits until `count` trees have ended as `outcome`, in all.
    async fn trees(&self, outcome: &str, count: u64) {
        let sample = format!(r#"tierstore_cache_hydrate_trees_total{{outcome="{outcome}"}}"#);
        until("the trees have ended", || self.metric(&sample) == count).await;
    }

    /// Waits until the workers have filled `count` blobs, in all.
    async fn filled(&self, count: u64) {
        until("the workers have filled the blobs", || {
            self.metric("tierstore_cache_prefetch_fills_total") == count
        })
        .await;
    }
}

/// Asserts that the answer to a hydrate call is made at the acceptance of the call: with the first
/// manifest held at the gate of the upstream, the call is answered `202` with the hold and the
/// number of trees, nothing of the tree is in the cache yet, and the expansion is at the manifest. It
/// would catch a handler that waits for the walk or for a fill before it answers.
#[tokio::test]
async fn hydrate_answers_202_while_the_first_manifest_is_still_held_at_the_store() {
    let f = fixture("hyd-202").await;
    let blob = stored(&f.cache, 1, 100).await;
    let root = manifest(&f.cache, &[blob.file("a")]).await;
    let gate = f.cache.tamper.hold_key(&key_of(&root));

    let reply = f.post("hydrate", &hydrate_body("h1", &[root])).await;
    assert_eq!(reply.status, StatusCode::ACCEPTED);
    assert_eq!(reply.header("content-type"), "application/json");
    assert_eq!(json_of(&reply), json!({"hold": "h1", "accepted": 1}));
    assert!(!f.cache.has_file(&key_of(&root)));
    until("the expansion asks the store for the manifest", || {
        f.cache.calls_on(Op::Get, &key_of(&root)) == 1
    })
    .await;
    assert!(!f.cache.has_file(&blob.key));
    assert_eq!(f.metric("tierstore_cache_holds_open"), 1);

    gate.add_permits(64);
    f.trees("expanded", 1).await;
    f.filled(1).await;
    assert!(f.cache.has_file(&blob.key));
    assert_eq!(f.metric("tierstore_cache_hydrate_requests_total"), 1);
}

/// Asserts that a blob is filled by ranged GETs of the chunk size and no HEAD: a blob of 10000
/// bytes with a chunk of 4096 takes three GETs of its key, and the upstream is asked for no HEAD at
/// all (the manifest gave the size). It would catch a fill that asks the store for the size.
#[tokio::test]
async fn hydrate_fills_a_blob_with_ranged_gets_and_no_head() {
    let f = fixture_with("hyd-ranged", |config| config.fill_chunk = 4096).await;
    let blob = stored(&f.cache, 1, 10_000).await;
    let root = manifest(&f.cache, &[blob.file("a")]).await;

    let reply = f.post("hydrate", &hydrate_body("h", &[root])).await;
    assert_eq!(reply.status, StatusCode::ACCEPTED);
    f.filled(1).await;
    assert_eq!(f.cache.calls_on(Op::Get, &blob.key), 3);
    assert_eq!(f.cache.calls_on(Op::Head, &blob.key), 0);
}

/// Asserts that a blob the cache holds is not fetched again: a blob read through the cache before
/// the call costs the upstream no request when its tree is hydrated, and the release counts it
/// `present`. It would catch a hydrate that fetches what is there.
#[tokio::test]
async fn hydrate_present_blob_costs_no_store_request() {
    let f = fixture("hyd-present").await;
    let blob = stored(&f.cache, 1, 100).await;
    let root = manifest(&f.cache, &[blob.file("a")]).await;
    let first = f.cache.get(&blob.key).await;
    assert_eq!(first.unwrap().len(), 100);
    let before = f.cache.calls_on(Op::Get, &blob.key);

    let reply = f.post("hydrate", &hydrate_body("h", &[root])).await;
    assert_eq!(reply.status, StatusCode::ACCEPTED);
    f.trees("expanded", 1).await;
    assert_eq!(f.cache.calls_on(Op::Get, &blob.key), before);

    let released = f.post("release", r#"{"hold":"h"}"#).await;
    let blobs = json_of(&released)["blobs"].clone();
    assert_eq!(
        (blobs["present"].clone(), blobs["filled"].clone()),
        (json!(1), json!(0))
    );
}

/// Asserts that bytes that are not the digest keep nothing: the object under the blob's key is
/// not what the manifest says, the fill fails, no file is left, and the release counts the blob
/// `failed`. It would catch a fill that keeps what it was given.
#[tokio::test]
async fn hydrate_blob_with_wrong_bytes_keeps_nothing() {
    let f = fixture("hyd-wrong").await;
    let blob = stored(&f.cache, 1, 100).await;
    f.cache.replace(&blob.key, &distinct(2, 100)).await;
    let root = manifest(&f.cache, &[blob.file("a")]).await;

    let reply = f.post("hydrate", &hydrate_body("h", &[root])).await;
    assert_eq!(reply.status, StatusCode::ACCEPTED);
    until("the fill failed", || {
        f.metric(r#"tierstore_cache_prefetch_fill_failures_total{reason="bad_digest"}"#) == 1
    })
    .await;
    assert!(!f.cache.has_file(&blob.key));
    let released = f.post("release", r#"{"hold":"h"}"#).await;
    assert_eq!(json_of(&released)["blobs"]["failed"], 1);
}

/// Asserts what a release says: a hold of three trees, one expanded into two blobs that were
/// filled, one whose manifest is not in the upstream (refused) and one that is cut by a bound,
/// is released with the counts of each, in exact equality; a second release of the same hold is
/// `404 NoSuchHold` with the message that names it. It would catch a release that miscounts, or that
/// answers for a hold that has ended.
#[tokio::test]
async fn release_answers_the_counts_of_its_hold_and_404_for_one_that_has_ended() {
    let f = fixture_with("hyd-release", |config| config.prefetch.max_blobs = 2).await;
    let (first, second) = (
        stored(&f.cache, 1, 100).await,
        stored(&f.cache, 2, 100).await,
    );
    let good = manifest(&f.cache, &[first.file("a"), second.file("b")]).await;
    let more = stored(&f.cache, 3, 100).await;
    let cut = manifest(&f.cache, &[more.file("c")]).await;
    let missing = TreeDigest::of(b"a manifest the upstream does not have");

    let trees = [good, missing, cut];
    let reply = f.post("hydrate", &hydrate_body("h", &trees)).await;
    assert_eq!(json_of(&reply), json!({"hold": "h", "accepted": 3}));
    f.filled(2).await;
    f.trees("expanded", 1).await;
    f.trees("refused", 1).await;
    f.trees("cut", 1).await;

    let released = f.post("release", r#"{"hold":"h"}"#).await;
    assert_eq!(released.status, StatusCode::OK);
    let expected = json!({
        "trees": 3, "expanded": 1, "cut": 1, "refused": 1,
        "blobs": {"filled": 2, "present": 0, "dropped": 0, "failed": 0, "pending": 0},
    });
    assert_eq!(json_of(&released), expected);
    assert_eq!(f.metric("tierstore_cache_holds_open"), 0);

    let again = f.post("release", r#"{"hold":"h"}"#).await;
    let message = r#"no hold "h" is open"#.to_owned();
    assert_eq!(error_of(&again), (404, "NoSuchHold".to_owned(), message));
}

/// Asserts the bounds of a call: more trees than `hydrate_max_trees` is `413 EntityTooLarge` with
/// the message that says how many and how many are allowed, and opens no hold; a body past what
/// that many digests take is `413` too, read no further. It would catch a call that opens a hold
/// and then refuses, or one that reads a body of any size.
#[tokio::test]
async fn a_call_over_max_trees_is_413_and_opens_no_hold() {
    let f = fixture_with("hyd-413", |config| config.prefetch.max_trees = 2).await;
    let trees = [
        TreeDigest::of(b"a"),
        TreeDigest::of(b"b"),
        TreeDigest::of(b"c"),
    ];
    let reply = f.post("hydrate", &hydrate_body("h", &trees)).await;
    let message = "3 trees in one call, at most 2".to_owned();
    assert_eq!(
        error_of(&reply),
        (413, "EntityTooLarge".to_owned(), message)
    );
    assert_eq!(f.metric("tierstore_cache_holds_open"), 0);

    let padded = format!(r#"{{"hold":"h","trees":[],"note":"{}"}}"#, "x".repeat(4096));
    let reply = f.post("hydrate", &padded).await;
    let message = "The request body is over the limit of 1280 bytes.".to_owned();
    assert_eq!(
        error_of(&reply),
        (413, "EntityTooLarge".to_owned(), message)
    );
    assert_eq!(f.metric("tierstore_cache_hydrate_requests_total"), 0);
}

/// Asserts that `hold_max_count` holds are open at most: the second hold is `429 TooManyRequests`
/// with a `Retry-After` and the count in its message, while the hold that is open takes more trees
/// under its own id. It would catch a bound that refuses an id that is open already.
#[tokio::test]
async fn a_hold_past_hold_max_count_is_429_and_an_open_hold_takes_more_trees() {
    let f = fixture_with("hyd-429", |config| config.prefetch.hold_max_count = 1).await;
    let one = hydrate_body("a", &[TreeDigest::of(b"1")]);
    assert_eq!(f.post("hydrate", &one).await.status, StatusCode::ACCEPTED);

    let other = hydrate_body("b", &[TreeDigest::of(b"1")]);
    let reply = f.post("hydrate", &other).await;
    assert_eq!(reply.header("retry-after"), "1");
    let message = "1 holds are open already".to_owned();
    assert_eq!(
        error_of(&reply),
        (429, "TooManyRequests".to_owned(), message)
    );

    let more = hydrate_body("a", &[TreeDigest::of(b"2")]);
    let reply = f.post("hydrate", &more).await;
    assert_eq!(json_of(&reply), json!({"hold": "a", "accepted": 1}));
}

/// Asserts the answer to each body that is not the shape, by its exact message: not JSON, no
/// `trees`, `trees` that is not an array, no `hold`, an empty id, an id of 129 bytes, a tree in
/// upper-case hex, a tree of the wrong length; and a release with no `hold`. A field the call does
/// not know is ignored, and a call with no trees is accepted with none. It would catch a handler
/// that accepts a body it cannot use, or refuses an additive field.
#[tokio::test]
async fn a_body_that_is_not_the_shape_is_400_and_an_unknown_field_is_ignored() {
    let f = fixture("hyd-400").await;
    let shape = "The body must be a JSON object with \"hold\" (a string), \"trees\" (an array of \
                 strings) and, optionally, \"expires_ms\" (a number).";
    let id = "The hold id must be 1 to 128 bytes.";
    let digit = "trees[0] is not the lower-case hex of a SHA-256.";
    let long = "h".repeat(129);
    let upper = "A".repeat(64);
    let short = "a".repeat(63);
    let cases = [
        ("not json".to_owned(), shape),
        (r#"{"hold":"h"}"#.to_owned(), shape),
        (r#"{"hold":"h","trees":"x"}"#.to_owned(), shape),
        (r#"{"trees":[]}"#.to_owned(), shape),
        (r#"{"hold":"","trees":[]}"#.to_owned(), id),
        (format!(r#"{{"hold":"{long}","trees":[]}}"#), id),
        (format!(r#"{{"hold":"h","trees":["{upper}"]}}"#), digit),
        (format!(r#"{{"hold":"h","trees":["{short}"]}}"#), digit),
    ];
    for (body, message) in cases {
        let reply = f.post("hydrate", &body).await;
        let wanted = (400, "InvalidRequest".to_owned(), message.to_owned());
        assert_eq!(error_of(&reply), wanted, "{body}");
    }
    let release = f.post("release", r#"{"trees":[]}"#).await;
    let message = "The body must be a JSON object with \"hold\" (a string).";
    let wanted = (400, "InvalidRequest".to_owned(), message.to_owned());
    assert_eq!(error_of(&release), wanted);
    let empty = f.post("release", r#"{"hold":""}"#).await;
    let wanted = (400, "InvalidRequest".to_owned(), id.to_owned());
    assert_eq!(error_of(&empty), wanted);

    let extra = r#"{"hold":"h","expires_ms":1000,"trees":[],"later":{"a":1}}"#;
    let reply = f.post("hydrate", extra).await;
    assert_eq!(json_of(&reply), json!({"hold": "h", "accepted": 0}));
}

/// Asserts that a tree listed twice is accepted once: the call says one tree, and the cache
/// expands it once. It would catch an `accepted` that counts the list, not the trees.
#[tokio::test]
async fn a_tree_listed_twice_is_accepted_once() {
    let f = fixture("hyd-dup").await;
    let blob = stored(&f.cache, 1, 100).await;
    let root = manifest(&f.cache, &[blob.file("a")]).await;
    let reply = f.post("hydrate", &hydrate_body("h", &[root, root])).await;
    assert_eq!(json_of(&reply), json!({"hold": "h", "accepted": 1}));
    f.trees("expanded", 1).await;
    assert_eq!(f.cache.calls_on(Op::Get, &key_of(&root)), 1);
}

/// Asserts that a release stops the expansion of its hold: the second manifest of a tree is held
/// at the gate, the hold is released, and when the gate opens the walk hands over no more blobs, the
/// tree counts as cut (not expanded), and the blob below the held manifest is never fetched. It
/// would catch an expansion that goes on for a hold that has ended.
#[tokio::test]
async fn release_stops_the_expansion_of_its_hold() {
    let f = fixture("hyd-stop").await;
    let (first, second) = (
        stored(&f.cache, 1, 100).await,
        stored(&f.cache, 2, 100).await,
    );
    let below = manifest(&f.cache, &[second.file("f")]).await;
    let dir = format!(r#"{{"name":"b","kind":"dir","sha256":"{below}","size":1}}"#);
    let root = manifest(&f.cache, &[first.file("a"), dir]).await;
    let gate = f.cache.tamper.hold_key(&key_of(&below));

    f.post("hydrate", &hydrate_body("h", &[root])).await;
    until("the walk is at the second manifest", || {
        f.cache.calls_on(Op::Get, &key_of(&below)) == 1
    })
    .await;
    let released = f.post("release", r#"{"hold":"h"}"#).await;
    assert_eq!(released.status, StatusCode::OK);

    gate.add_permits(64);
    f.trees("cut", 1).await;
    assert_eq!(f.cache.calls_on(Op::Get, &second.key), 0);
    assert_eq!(
        f.metric(r#"tierstore_cache_hydrate_trees_total{outcome="expanded"}"#),
        0
    );
}

/// Asserts that a node with no local cache answers `501 HydrateNotAvailable` to both calls, and
/// so does a cache whose `prefetch_concurrency` is 0, each with its own message. It would catch a
/// node that falls through to the bucket's answer, or one that opens holds it cannot fill.
#[tokio::test]
async fn a_node_that_cannot_hydrate_answers_501_hydrate_not_available() {
    let storage = Rig::new(1 << 20).await;
    let message = "This tierstore does not take hydrate calls: the prefetch queue is off: \
                   prefetch_concurrency is 0.";
    let wanted = (501, "HydrateNotAvailable".to_owned(), message.to_owned());
    for call in ["hydrate", "release"] {
        let path = format!("/_tierstore/v1/{call}");
        let reply = storage.send("POST", &path, &[], b"{}").await;
        assert_eq!(error_of(&reply), wanted, "{call}");
    }
    let off = fixture_with("hyd-off", |config| config.prefetch.concurrency = 0).await;
    for call in ["hydrate", "release"] {
        let reply = off.post(call, "{}").await;
        assert_eq!(error_of(&reply), wanted, "{call}");
    }
}

/// Asserts that a cache that does not hold the lock of its directory answers both calls
/// `503 SlowDown` with a `Retry-After`, and opens no hold. It would catch a read-only instance
/// that takes holds it can never fill or pin.
#[tokio::test]
async fn a_read_only_cache_answers_503_slow_down() {
    let held = CacheRig::with("hyd-ro", |config| config.prefetch.concurrency = 1);
    let _writer = held.locked();
    let reader = held.locked();
    assert_eq!(reader.mode(), Mode::ReadOnly);
    let server = Rig::with(1 << 20, move |config| config.cache = Some(reader)).await;
    let f = Fixture {
        cache: held,
        server,
    };
    let message = "This tierstore does not hold the lock of its cache directory now; retry after \
                   a moment.";
    let wanted = (503, "SlowDown".to_owned(), message.to_owned());
    for call in ["hydrate", "release"] {
        let reply = f.post(call, r#"{"hold":"h","trees":[]}"#).await;
        assert_eq!(reply.header("retry-after"), "1");
        assert_eq!(error_of(&reply), wanted, "{call}");
    }
}

/// Asserts the status and code each failure of the cache is told as, including the two that a valid
/// body cannot reach (a tree the hold is not for, a blob that is not a digest), which are
/// `400 InvalidRequest`. It would catch a failure that is answered as an internal error.
#[test]
fn every_failure_of_the_cache_has_its_own_status_and_code() {
    let no_tree = HoldError::NoSuchTree {
        hold: "h".to_owned(),
        tree: "t".to_owned(),
    };
    let kinds = [
        (HydrateError::Off, (501, "HydrateNotAvailable")),
        (HoldError::ReadOnly.into(), (503, "SlowDown")),
        (HoldError::TooMany(2).into(), (429, "TooManyRequests")),
        (
            HoldError::NoSuchHold("h".to_owned()).into(),
            (404, "NoSuchHold"),
        ),
        (
            TooManyTrees { given: 3, max: 2 }.into(),
            (413, "EntityTooLarge"),
        ),
        (no_tree.into(), (400, "InvalidRequest")),
        (
            HoldError::NotADigest("x".to_owned()).into(),
            (400, "InvalidRequest"),
        ),
    ];
    for (error, (status, code)) in kinds {
        let said = refused(&error);
        assert_eq!((said.kind().0.as_u16(), said.kind().1), (status, code));
    }
}

/// Asserts that a read-only key may hydrate and release, as the design says: it gains no right to
/// read anything it could not, and the call is answered as for the full key. It would catch a role
/// check that refuses a `POST` the key makes here.
#[tokio::test]
async fn read_only_key_may_hydrate() {
    let f = fixture("hyd-reader").await;
    let blob = stored(&f.cache, 1, 100).await;
    let root = manifest(&f.cache, &[blob.file("a")]).await;
    let body = hydrate_body("h", &[root]);
    let reply = f.post_as(READER, "hydrate", &body).await;
    assert_eq!(json_of(&reply), json!({"hold": "h", "accepted": 1}));
    f.filled(1).await;
    let released = f.post_as(READER, "release", r#"{"hold":"h"}"#).await;
    assert_eq!(released.status, StatusCode::OK);
}

/// Asserts that a call that is not signed with a header is refused: a POST of the path signed
/// as a presigned URL is `403 AccessDenied`, and a request not signed at all is refused by the
/// signature check before the call is looked at; neither opens a hold. It would catch a call that a
/// URL good for one object can make.
#[tokio::test]
async fn a_presigned_url_cannot_hydrate() {
    let f = fixture("hyd-presigned").await;
    let key = AccessKey::new(KEY_ID, SECRET);
    let request = PresignRequest {
        endpoint: "http://tier.test",
        region: "us-east-1",
        bucket: "_tierstore",
        key: "v1/hydrate",
        expires_secs: 300,
        now: NOW,
    };
    let url = presign_url("POST", &key, &request, &[]).unwrap();
    let rest = url.split_once("://").unwrap().1;
    let at = rest.find('/').unwrap();
    let reply = f.server.send_raw(unsigned(&rest[at..], &rest[..at])).await;
    assert_eq!(
        (reply.status, reply.code()),
        (StatusCode::FORBIDDEN, "AccessDenied".to_owned())
    );
    let bare = unsigned("/_tierstore/v1/hydrate", "tier.test");
    let reply = f.server.send_raw(bare).await;
    assert_eq!(
        (reply.status, reply.code()),
        (StatusCode::FORBIDDEN, "AccessDenied".to_owned())
    );
    assert_eq!(f.metric("tierstore_cache_holds_open"), 0);
}

/// Asserts that a scoped key expands only what it may read: with a read prefix `cas/l/`, a file
/// whose `source.key` is under `cas/f/` is left out and counted `access_denied` while the file of
/// the whole-object key is queued and filled; a key whose prefix is `cas/f/` alone is not allowed
/// the root manifest, so its tree is counted `access_denied`, the manifest is never asked of the
/// upstream, and nothing is queued; a scoped key with no read prefix is `403`. It would catch a
/// call that lets a key reach through a manifest what it cannot reach itself.
#[tokio::test]
async fn scoped_key_is_checked_per_manifest_and_per_blob() {
    let f = fixture("hyd-scoped").await;
    let near = stored(&f.cache, 1, 100).await;
    let far = stored(&f.cache, 2, 100).await;
    let source = format!(
        r#"{{"name":"b","kind":"file","sha256":"{}","size":100,"source":{{"key":"cas/f/pack","offset":0}}}}"#,
        far.sha
    );
    let root = manifest(&f.cache, &[near.file("a"), source]).await;

    let reply = f
        .post_as(NARROW, "hydrate", &hydrate_body("n", &[root]))
        .await;
    assert_eq!(reply.status, StatusCode::ACCEPTED);
    f.trees("expanded", 1).await;
    f.filled(1).await;
    assert!(f.cache.has_file(&near.key));
    assert_eq!(f.cache.calls_on(Op::Get, "cas/f/pack"), 0);
    let blobs = r#"tierstore_cache_hydrate_blobs_total{outcome="#;
    assert_eq!(f.metric(&format!(r#"{blobs}"access_denied"}}"#)), 1);
    assert_eq!(f.metric(&format!(r#"{blobs}"queued"}}"#)), 1);

    let before = f.cache.calls_on(Op::Get, &key_of(&root));
    let reply = f
        .post_as(WIDE, "hydrate", &hydrate_body("w", &[root]))
        .await;
    assert_eq!(reply.status, StatusCode::ACCEPTED);
    f.trees("access_denied", 1).await;
    assert_eq!(f.cache.calls_on(Op::Get, &key_of(&root)), before);
    assert_eq!(f.metric(&format!(r#"{blobs}"queued"}}"#)), 1);

    let reply = f
        .post_as(WRITER, "hydrate", &hydrate_body("x", &[root]))
        .await;
    assert_eq!(
        (reply.status, reply.code()),
        (StatusCode::FORBIDDEN, "AccessDenied".to_owned())
    );
}

/// Asserts which requests are the calls: a `POST` of exactly the two paths, and nothing else.
#[test]
fn only_a_post_of_the_two_paths_is_a_call() {
    let hydrate = "/_tierstore/v1/hydrate";
    let release = "/_tierstore/v1/release";
    assert_eq!(asked(&Method::POST, hydrate), Some(Call::Hydrate));
    assert_eq!(asked(&Method::POST, release), Some(Call::Release));
    assert_eq!(asked(&Method::GET, hydrate), None);
    assert_eq!(asked(&Method::POST, "/_tierstore/v1/hydrate/"), None);
    assert_eq!(
        asked(&Method::POST, "/tier-bucket/_tierstore/v1/hydrate"),
        None
    );
}

/// Asserts the counters of a scripted run: one accepted call, a hold open until the release, two
/// trees expanded, and two blobs queued and filled.
#[tokio::test]
async fn the_counters_of_the_calls_after_a_scripted_run() {
    let f = fixture("hyd-counters").await;
    let blobs = [
        stored(&f.cache, 1, 100).await,
        stored(&f.cache, 2, 100).await,
    ];
    let one = manifest(&f.cache, &[blobs[0].file("a")]).await;
    let two = manifest(&f.cache, &[blobs[1].file("a")]).await;
    f.post("hydrate", &hydrate_body("h", &[one, two])).await;
    f.trees("expanded", 2).await;
    f.filled(2).await;
    assert_eq!(f.metric("tierstore_cache_hydrate_requests_total"), 1);
    assert_eq!(f.metric("tierstore_cache_holds_open"), 1);
    let queued = r#"tierstore_cache_hydrate_blobs_total{outcome="queued"}"#;
    assert_eq!(f.metric(queued), 2);
    f.post("release", r#"{"hold":"h"}"#).await;
    assert_eq!(f.metric("tierstore_cache_holds_open"), 0);
}
