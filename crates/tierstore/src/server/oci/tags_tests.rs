//! Tags of the OCI front: pushed, pulled, listed and moved, over an in-memory store, with no
//! socket. A tag is a directory of immutable versions, and a move writes the next one. Two
//! pushes of one tag are made to meet at the store with a barrier, not with time.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use bytes::Bytes;
use hyper::{Method, StatusCode};
use serde_json::Value;
use tokio::sync::Barrier;

use super::names::{Route, route};
use super::push_manifest_tests::{image, stocked};
use super::push_tests::{Answer, Rig, digest_of, front_over, full};
use crate::{Backend, ByteRange, Error, FaultyBackend, Listed, MemoryBackend, Meta, Op};

/// The key of the version of `tag` of the name `app` that follows `moves` earlier ones (0 for the
/// first version, whose suffix is `u64::MAX`).
pub(super) fn version(tag: &str, moves: u64) -> String {
    format!("oci/tags/app/{tag}/{:020}", u64::MAX - moves)
}

/// A store whose listings of a tag, once armed, wait for a second reader before they answer, so
/// that two pushes of one tag have both read its newest version before either writes.
pub(super) struct Gated {
    pub(super) inner: Arc<dyn Backend>,
    pub(super) barrier: Arc<Barrier>,
    pub(super) armed: AtomicBool,
}

#[async_trait]
impl Backend for Gated {
    async fn put(&self, key: &str, body: Bytes) -> Result<(), Error> {
        self.inner.put(key, body).await
    }

    async fn put_if_match(&self, key: &str, body: Bytes, etag: &str) -> Result<String, Error> {
        self.inner.put_if_match(key, body, etag).await
    }

    async fn get(&self, key: &str, range: Option<ByteRange>) -> Result<Bytes, Error> {
        self.inner.get(key, range).await
    }

    async fn head(&self, key: &str) -> Result<Meta, Error> {
        self.inner.head(key).await
    }

    async fn delete(&self, key: &str) -> Result<(), Error> {
        self.inner.delete(key).await
    }

    async fn list(&self, prefix: &str) -> Result<Vec<Listed>, Error> {
        self.inner.list(prefix).await
    }

    async fn list_page(
        &self,
        prefix: &str,
        start_after: Option<&str>,
        max_keys: usize,
    ) -> Result<Vec<Listed>, Error> {
        let page = self.inner.list_page(prefix, start_after, max_keys).await;
        if prefix.contains("/tags/") && self.armed.load(Ordering::SeqCst) {
            self.barrier.wait().await;
        }
        page
    }
}

impl Gated {
    pub(super) fn over(inner: Arc<dyn Backend>, barrier: Arc<Barrier>) -> Self {
        Self {
            inner,
            barrier,
            armed: AtomicBool::new(false),
        }
    }
}

/// A store whose next create of a version of a tag is answered with `say` after `plant`, if
/// there is one, was stored at the key: a create that landed and was not told, or one that a
/// concurrent writer won.
struct Meddle {
    inner: Arc<dyn Backend>,
    plan: Mutex<Option<(Option<Bytes>, Error)>>,
}

impl Meddle {
    fn arm(&self, plant: Option<&str>, say: Error) {
        let plant = plant.map(|digest| Bytes::copy_from_slice(digest.as_bytes()));
        *self.plan.lock().expect("a lock") = Some((plant, say));
    }
}

#[async_trait]
impl Backend for Meddle {
    async fn put(&self, key: &str, body: Bytes) -> Result<(), Error> {
        let planned = if key.contains("/tags/") {
            self.plan.lock().expect("a lock").take()
        } else {
            None
        };
        let Some((plant, say)) = planned else {
            return self.inner.put(key, body).await;
        };
        if let Some(plant) = plant {
            self.inner.put(key, plant).await?;
        }
        Err(say)
    }

    async fn put_if_match(&self, key: &str, body: Bytes, etag: &str) -> Result<String, Error> {
        self.inner.put_if_match(key, body, etag).await
    }

    async fn get(&self, key: &str, range: Option<ByteRange>) -> Result<Bytes, Error> {
        self.inner.get(key, range).await
    }

    async fn head(&self, key: &str) -> Result<Meta, Error> {
        self.inner.head(key).await
    }

    async fn delete(&self, key: &str) -> Result<(), Error> {
        self.inner.delete(key).await
    }

    async fn list(&self, prefix: &str) -> Result<Vec<Listed>, Error> {
        self.inner.list(prefix).await
    }
}

/// A rig whose front is over the store that `wrap` makes of the fault injector, and that store.
fn rig_over<B: Backend + 'static>(
    wrap: impl FnOnce(Arc<FaultyBackend>) -> Arc<B>,
) -> (Rig, Arc<B>) {
    let memory = Arc::new(MemoryBackend::new());
    let faulty = Arc::new(FaultyBackend::new(memory.clone()));
    let wrapped = wrap(faulty.clone());
    let (front, metrics) = front_over(wrapped.clone());
    let rig = Rig {
        faulty,
        memory,
        front,
        metrics,
    };
    (rig, wrapped)
}

/// A rig over a gate, and the gate.
fn gated() -> (Rig, Arc<Gated>) {
    rig_over(|faulty| Arc::new(Gated::over(faulty, Arc::new(Barrier::new(2)))))
}

/// A rig over a store that can answer a create of a version as it is told to, and that store.
fn meddled() -> (Rig, Arc<Meddle>) {
    rig_over(|faulty| {
        Arc::new(Meddle {
            inner: faulty,
            plan: Mutex::new(None),
        })
    })
}

impl Rig {
    /// Pushes `bytes` under `tag` of `name`.
    pub(super) async fn tag(&self, name: &str, tag: &str, bytes: &[u8]) -> Answer {
        let target = format!("/v2/{name}/manifests/{tag}");
        self.ask(Method::PUT, &target, &[], full(bytes)).await
    }

    /// The tags the front lists for `name`, with the query `query` (empty for none).
    async fn tags(&self, name: &str, query: &str) -> Answer {
        let target = format!("/v2/{name}/tags/list{query}");
        self.send(Method::GET, &target).await
    }
}

fn listed(answer: &Answer) -> Vec<String> {
    let document: Value = serde_json::from_slice(&answer.body).expect("a document");
    let tags = document["tags"].as_array().expect("tags");
    tags.iter()
        .map(|tag| tag.as_str().expect("text").to_owned())
        .collect()
}

/// `count` manifests of one rig that differ in their bytes, as `(bytes, digest)`.
pub(super) async fn manifests(rig: &Rig, count: usize) -> Vec<(Vec<u8>, String)> {
    let config = rig.seed(b"{}").await;
    let mut made = Vec::new();
    for number in 0..count {
        let layer = rig.seed(format!("layer {number}").as_bytes()).await;
        let bytes = image(&config, &[layer]);
        let digest = digest_of(&bytes);
        made.push((bytes, digest));
    }
    made
}

/// Asserts that a manifest pushed under a tag is pulled by the tag (GET and HEAD) with its bytes,
/// media type and digest, that the tag is one version under the reserved directory of its name
/// (a name with slashes written with `~`), and that a push by digest makes no tag.
#[tokio::test]
async fn a_pushed_tag_is_pulled_by_the_tag() {
    let (rig, config, layer) = stocked().await;
    let bytes = image(&config, &[&layer]);
    let digest = digest_of(&bytes);
    let put = rig.tag("lib/app", "v1", &bytes).await;
    assert_eq!(put.status, StatusCode::CREATED, "{}", put.text());
    assert_eq!(put.header("docker-content-digest"), digest);
    let key = "oci/tags/lib~app/v1/18446744073709551615";
    let held = rig.memory.get(key, None).await;
    assert_eq!(held.expect("a version"), digest.as_bytes());
    let got = rig.send(Method::GET, "/v2/lib/app/manifests/v1").await;
    assert_eq!(got.status, StatusCode::OK, "{}", got.text());
    assert_eq!(got.body, bytes);
    assert_eq!(got.header("docker-content-digest"), digest);
    assert_eq!(
        got.header("content-type"),
        "application/vnd.oci.image.manifest.v1+json"
    );
    let head = rig.send(Method::HEAD, "/v2/lib/app/manifests/v1").await;
    assert_eq!(head.status, StatusCode::OK);
    assert!(head.body.is_empty());
    assert_eq!(head.header("content-length"), bytes.len().to_string());
    let target = format!("/v2/lib/app/manifests/{digest}");
    let by_digest = rig.ask(Method::PUT, &target, &[], full(&bytes)).await;
    assert_eq!(by_digest.status, StatusCode::CREATED);
    assert_eq!(listed(&rig.tags("lib/app", "").await), ["v1"]);
}

/// Asserts that a tag that is pushed again points at the new manifest, and the old manifest is
/// still pulled by its digest.
#[tokio::test]
async fn a_tag_pushed_again_moves() {
    let rig = Rig::new();
    let made = manifests(&rig, 2).await;
    let ((first, first_digest), (second, second_digest)) = (made[0].clone(), made[1].clone());
    assert_eq!(
        rig.tag("app", "latest", &first).await.status,
        StatusCode::CREATED
    );
    assert_eq!(
        rig.tag("app", "latest", &second).await.status,
        StatusCode::CREATED
    );
    let by_tag = rig.send(Method::GET, "/v2/app/manifests/latest").await;
    assert_eq!(by_tag.body, second);
    assert_eq!(by_tag.header("docker-content-digest"), second_digest);
    let old = rig
        .send(Method::GET, &format!("/v2/app/manifests/{first_digest}"))
        .await;
    assert_eq!(old.body, first);
}

/// Asserts that after three pushes of one tag there are three version objects, in the listing
/// newest first (the suffix counts down from `u64::MAX`), and that the first one is byte for byte
/// what it was after the first push: a move writes a new object and overwrites none.
#[tokio::test]
async fn a_tag_move_writes_a_new_object_and_overwrites_none() {
    let rig = Rig::new();
    let made = manifests(&rig, 3).await;
    let first_key = version("latest", 0);
    let mut seen = Vec::new();
    for (bytes, _) in &made {
        assert_eq!(
            rig.tag("app", "latest", bytes).await.status,
            StatusCode::CREATED
        );
        seen.push(rig.memory.get(&first_key, None).await.expect("a version"));
    }
    let keys: Vec<String> = rig
        .memory
        .list("oci/tags/app/latest/")
        .await
        .expect("a listing")
        .into_iter()
        .map(|object| object.key)
        .collect();
    let want: Vec<String> = (0..3).map(|moves| version("latest", moves)).collect();
    assert_eq!(keys, [want[2].clone(), want[1].clone(), want[0].clone()]);
    for first in &seen {
        assert_eq!(first, made[0].1.as_bytes());
    }
    for (moves, (_, digest)) in made.iter().enumerate() {
        let held = rig.memory.get(&want[moves], None).await;
        assert_eq!(held.expect("a version"), digest.as_bytes());
    }
}

/// Asserts that a pull by tag serves the newest version: the last manifest pushed, also when it
/// is one the tag held before (a push of an older digest is a move, not a no-op).
#[tokio::test]
async fn the_newest_version_is_the_tag() {
    let rig = Rig::new();
    let made = manifests(&rig, 2).await;
    for index in [0, 1, 0] {
        let (bytes, digest) = &made[index];
        assert_eq!(
            rig.tag("app", "latest", bytes).await.status,
            StatusCode::CREATED
        );
        let by_tag = rig.send(Method::GET, "/v2/app/manifests/latest").await;
        assert_eq!(&by_tag.body, bytes);
        assert_eq!(by_tag.header("docker-content-digest"), digest.as_str());
    }
    let head = rig.send(Method::HEAD, "/v2/app/manifests/latest").await;
    assert_eq!(head.header("docker-content-digest"), made[0].1);
    let held = rig.memory.list("oci/tags/app/latest/").await;
    assert_eq!(held.expect("a listing").len(), 3);
}

/// Asserts that a push of the digest the tag already holds is 201 and writes a version of its
/// own, and that the tag still points at the digest after the version it found is removed (as
/// the proxy removes a version whose push then fails to journal): a 201 is never answered from
/// a version another push made.
#[tokio::test]
async fn a_push_of_the_digest_the_tag_holds_writes_a_version_of_its_own() {
    let rig = Rig::new();
    let made = manifests(&rig, 1).await;
    let (bytes, digest) = (&made[0].0, &made[0].1);
    rig.memory
        .put(&version("latest", 0), Bytes::from(digest.clone()))
        .await
        .expect("put");
    let put = rig.tag("app", "latest", bytes).await;
    assert_eq!(put.status, StatusCode::CREATED, "{}", put.text());
    let own = rig.memory.get(&version("latest", 1), None).await;
    assert_eq!(own.expect("a version of its own"), digest.as_bytes());
    rig.memory
        .delete(&version("latest", 0))
        .await
        .expect("a delete");
    let by_tag = rig.send(Method::GET, "/v2/app/manifests/latest").await;
    assert_eq!(&by_tag.body, bytes);
    for _ in 0..2 {
        let again = rig.tag("app", "latest", bytes).await;
        assert_eq!(again.status, StatusCode::CREATED, "{}", again.text());
    }
    let held = rig.memory.list("oci/tags/app/latest/").await;
    assert_eq!(held.expect("a listing").len(), 3);
}

/// Asserts that of two pushes of a tag that does not exist, which both found it without a version
/// before either wrote, exactly one is 201 and the tag points at its manifest, and the other is
/// 409 `TAG_CONFLICT` and leaves the tag alone.
#[tokio::test]
async fn two_pushes_of_a_new_tag_have_one_winner() {
    let (rig, gate) = gated();
    let made = manifests(&rig, 2).await;
    let ((first, first_digest), (second, second_digest)) = (made[0].clone(), made[1].clone());
    gate.armed.store(true, Ordering::SeqCst);
    let (one, two) = tokio::join!(
        rig.tag("app", "latest", &first),
        rig.tag("app", "latest", &second)
    );
    gate.armed.store(false, Ordering::SeqCst);
    let won = [(one, first_digest), (two, second_digest)];
    let winners: Vec<&(Answer, String)> = won
        .iter()
        .filter(|(answer, _)| answer.status == StatusCode::CREATED)
        .collect();
    assert_eq!(winners.len(), 1);
    let losers: Vec<&(Answer, String)> = won
        .iter()
        .filter(|(answer, _)| answer.status != StatusCode::CREATED)
        .collect();
    let (message, detail) = losers[0]
        .0
        .assert_error(StatusCode::CONFLICT, "TAG_CONFLICT");
    assert!(
        message.contains("another push moved this tag first"),
        "{message}"
    );
    assert_eq!(detail["tag"], "latest");
    let held = rig.memory.get(&version("latest", 0), None).await;
    assert_eq!(held.expect("a version"), winners[0].1.as_bytes());
    let all = rig.memory.list("oci/tags/app/latest/").await;
    assert_eq!(all.expect("a listing").len(), 1);
    assert_eq!(rig.metrics.requests("manifest", "created"), 1);
    assert_eq!(rig.metrics.requests("manifest", "conflict"), 1);
}

/// Asserts that of two pushes of a tag that exists, which both read its newest version before
/// either wrote, exactly one is 201 and the other is 409; the manifest of the loser is stored by
/// its digest, the tag points at the winner's, and a push of the loser's manifest after that is
/// taken as the next version.
#[tokio::test]
async fn two_pushes_of_an_existing_tag_have_one_winner() {
    let (rig, gate) = gated();
    let made = manifests(&rig, 3).await;
    let (first, second, third) = (&made[0].0, &made[1].0, &made[2].0);
    let (first_digest, second_digest, third_digest) = (&made[0].1, &made[1].1, &made[2].1);
    assert_eq!(
        rig.tag("app", "latest", first).await.status,
        StatusCode::CREATED
    );
    gate.armed.store(true, Ordering::SeqCst);
    let (one, two) = tokio::join!(
        rig.tag("app", "latest", second),
        rig.tag("app", "latest", third)
    );
    gate.armed.store(false, Ordering::SeqCst);
    let (winner, loser) = match (one.status, two.status) {
        (StatusCode::CREATED, StatusCode::CONFLICT) => (second_digest, third),
        (StatusCode::CONFLICT, StatusCode::CREATED) => (third_digest, second),
        other => panic!("not one winner and one loser: {other:?}"),
    };
    let held = rig.memory.get(&version("latest", 1), None).await;
    assert_eq!(held.expect("a version"), winner.as_bytes());
    assert_ne!(first_digest, winner);
    let again = rig.tag("app", "latest", loser).await;
    assert_eq!(again.status, StatusCode::CREATED, "{}", again.text());
    let by_tag = rig.send(Method::GET, "/v2/app/manifests/latest").await;
    assert_eq!(&by_tag.body, loser);
}

/// Asserts that two pushes of one digest to a tag, which both read the tag without a version
/// before either wrote, are both 201: the loser of the create reads the winner's version back,
/// finds the digest it asked for, and writes the version after it, which it owns. Two versions
/// are stored, both holding the digest.
#[tokio::test]
async fn two_pushes_of_one_digest_are_both_201() {
    let (rig, gate) = gated();
    let bytes = manifests(&rig, 1).await.remove(0).0;
    gate.armed.store(true, Ordering::SeqCst);
    let (one, two) = tokio::join!(
        rig.tag("app", "latest", &bytes),
        rig.tag("app", "latest", &bytes)
    );
    gate.armed.store(false, Ordering::SeqCst);
    assert_eq!(one.status, StatusCode::CREATED, "{}", one.text());
    assert_eq!(two.status, StatusCode::CREATED, "{}", two.text());
    let all = rig.memory.list("oci/tags/app/latest/").await;
    assert_eq!(all.expect("a listing").len(), 2);
    for moves in 0..2 {
        let held = rig.memory.get(&version("latest", moves), None).await;
        assert_eq!(held.expect("a version"), digest_of(&bytes).as_bytes());
    }
}

/// Asserts what a push does when the create of its version is not clear from the store's answer
/// (`Unavailable`, as an S3 conflict is, or `AlreadyExists`), by reading the version back: the
/// digest it asked for is 201 after `Unavailable` and, after `AlreadyExists`, 201 with a version
/// of its own after the taken one (the taken one may be removed by its pusher), another digest is 409 `TAG_CONFLICT` and is never the tag, and a
/// version that is not there is 503 after `Unavailable` (try again) and 409 after `AlreadyExists`.
#[tokio::test]
async fn an_unclear_create_is_settled_by_reading_the_version_back() {
    let (rig, meddle) = meddled();
    let made = manifests(&rig, 2).await;
    let ((mine, mine_digest), (_, other_digest)) = (made[0].clone(), made[1].clone());
    let down = || Error::Unavailable("conflict".to_owned());
    let taken = || Error::AlreadyExists("version".to_owned());
    let cases = [
        ("u-landed", down(), Some(&mine_digest), StatusCode::CREATED),
        ("u-other", down(), Some(&other_digest), StatusCode::CONFLICT),
        ("u-absent", down(), None, StatusCode::SERVICE_UNAVAILABLE),
        ("a-landed", taken(), Some(&mine_digest), StatusCode::CREATED),
        (
            "a-other",
            taken(),
            Some(&other_digest),
            StatusCode::CONFLICT,
        ),
        ("a-absent", taken(), None, StatusCode::CONFLICT),
    ];
    for (tag, say, plant, status) in cases {
        meddle.arm(plant.map(String::as_str), say);
        let put = rig.tag("app", tag, &mine).await;
        assert_eq!(put.status, status, "{tag}: {}", put.text());
        let held = rig.memory.get(&version(tag, 0), None).await;
        match plant {
            None => assert!(held.is_err(), "{tag}"),
            Some(planted) => assert_eq!(held.expect("a version"), planted.as_bytes()),
        }
        if tag == "a-landed" {
            let own = rig.memory.get(&version(tag, 1), None).await;
            assert_eq!(own.expect("a version of its own"), mine_digest.as_bytes());
            rig.memory.delete(&version(tag, 0)).await.expect("a delete");
            let by_tag = rig.send(Method::GET, "/v2/app/manifests/a-landed").await;
            assert_eq!(&by_tag.body, &mine);
        }
    }
    assert_eq!(rig.metrics.requests("manifest", "conflict"), 3);
}

/// Asserts that a push that loses the create of the second to last version to a push of its own
/// digest, and so has no version left to write after it, is 500 `UNKNOWN` and not a 201 from the
/// other push's version.
#[tokio::test]
async fn a_loser_with_no_version_left_to_write_is_500() {
    let (rig, meddle) = meddled();
    let made = manifests(&rig, 2).await;
    let ((mine, mine_digest), (_, other_digest)) = (made[0].clone(), made[1].clone());
    let second_to_last = "oci/tags/app/edge/00000000000000000001";
    rig.memory
        .put(second_to_last, Bytes::from(other_digest))
        .await
        .expect("put");
    meddle.arm(
        Some(mine_digest.as_str()),
        Error::AlreadyExists("version".to_owned()),
    );
    let put = rig.tag("app", "edge", &mine).await;
    put.assert_error(StatusCode::INTERNAL_SERVER_ERROR, "UNKNOWN");
}

/// Asserts that a create answered `Unavailable` whose read-back fails too is 503, and that a
/// create answered `AlreadyExists` whose version is not a digest is 500 `UNKNOWN`, not a win.
#[tokio::test]
async fn a_failing_read_back_is_the_failure_of_the_push() {
    let (rig, meddle) = meddled();
    let bytes = manifests(&rig, 1).await.remove(0).0;
    let down = Error::Unavailable("down".to_owned());
    meddle.arm(None, down.clone());
    rig.faulty
        .fail_next_on(Op::Get, "oci/tags/app/first/", down.clone());
    let put = rig.tag("app", "first", &bytes).await;
    put.assert_error(StatusCode::SERVICE_UNAVAILABLE, "UNAVAILABLE");
    meddle.arm(Some("nope"), Error::AlreadyExists("version".to_owned()));
    let put = rig.tag("app", "second", &bytes).await;
    put.assert_error(StatusCode::INTERNAL_SERVER_ERROR, "UNKNOWN");
}

/// Asserts that a store that fails while a tag is read or written is 503 `UNAVAILABLE`, on the
/// listing of the versions, on the read of the newest, and on the create (the version is then
/// not there, so the push may be tried again), and that a version that is not a digest is 500
/// `UNKNOWN` and is not served.
#[tokio::test]
async fn a_store_that_fails_under_a_tag_is_503() {
    let rig = Rig::new();
    let made = manifests(&rig, 2).await;
    let (first, second) = (made[0].0.clone(), made[1].0.clone());
    let down = Error::Unavailable("down".to_owned());
    rig.faulty.fail_next_on(Op::List, "oci/tags/", down.clone());
    let read = rig.tag("app", "latest", &first).await;
    read.assert_error(StatusCode::SERVICE_UNAVAILABLE, "UNAVAILABLE");
    assert_eq!(
        rig.tag("app", "latest", &first).await.status,
        StatusCode::CREATED
    );
    rig.faulty
        .fail_next_on(Op::Get, "oci/tags/app/latest/", down.clone());
    let newest = rig.tag("app", "latest", &second).await;
    newest.assert_error(StatusCode::SERVICE_UNAVAILABLE, "UNAVAILABLE");
    rig.faulty.fail_next_on(Op::Put, "oci/tags/", down);
    let create = rig.tag("app", "other", &first).await;
    create.assert_error(StatusCode::SERVICE_UNAVAILABLE, "UNAVAILABLE");
    let bad = rig
        .memory
        .put(&version("bad", 0), Bytes::from_static(b"nope"))
        .await;
    bad.expect("put");
    let served = rig.send(Method::GET, "/v2/app/manifests/bad").await;
    served.assert_error(StatusCode::INTERNAL_SERVER_ERROR, "UNKNOWN");
}

/// Asserts that a version that is a valid digest followed by more bytes is 500 `UNKNOWN` and is
/// not served, as a version of exactly the digest's length is the only one that is a tag.
#[tokio::test]
async fn a_tag_object_longer_than_a_digest_is_not_served() {
    let rig = Rig::new();
    let made = manifests(&rig, 1).await;
    let digest = made[0].1.clone();
    rig.memory
        .put(&version("long", 0), Bytes::from(format!("{digest}garbage")))
        .await
        .expect("put");
    let served = rig.send(Method::GET, "/v2/app/manifests/long").await;
    served.assert_error(StatusCode::INTERNAL_SERVER_ERROR, "UNKNOWN");
}

/// Asserts that an object in a tag's directory whose name does not end in the 20 digits of a
/// version (too short, not digits, or too big for a `u64`) is 500 `UNKNOWN` for a pull and for a
/// push, and that the last version (suffix 0) is 500 for a push of any digest, the one it holds
/// too, as there is no version after it to write.
#[tokio::test]
async fn a_name_that_is_no_version_is_refused_and_the_last_version_cannot_be_moved() {
    let rig = Rig::new();
    let made = manifests(&rig, 2).await;
    let ((first, first_digest), (second, _)) = (made[0].clone(), made[1].clone());
    let odd = [
        ("junk", "oci/tags/app/junk/junk"),
        ("letters", "oci/tags/app/letters/abcdefghijklmnopqrst"),
        ("big", "oci/tags/app/big/99999999999999999999"),
    ];
    for (tag, key) in odd {
        rig.memory
            .put(key, Bytes::from(first_digest.clone()))
            .await
            .expect("put");
        let pull = rig
            .send(Method::GET, &format!("/v2/app/manifests/{tag}"))
            .await;
        pull.assert_error(StatusCode::INTERNAL_SERVER_ERROR, "UNKNOWN");
        let push = rig.tag("app", tag, &second).await;
        push.assert_error(StatusCode::INTERNAL_SERVER_ERROR, "UNKNOWN");
    }
    let last = "oci/tags/app/end/00000000000000000000";
    rig.memory
        .put(last, Bytes::from(first_digest.clone()))
        .await
        .expect("put");
    let moved = rig.tag("app", "end", &second).await;
    moved.assert_error(StatusCode::INTERNAL_SERVER_ERROR, "UNKNOWN");
    let same = rig.tag("app", "end", &first).await;
    same.assert_error(StatusCode::INTERNAL_SERVER_ERROR, "UNKNOWN");
}

/// Asserts that a version that is listed and then not there (a read that answers not found) is
/// looked for once more and the pull is served, and that a second miss is 404 `MANIFEST_UNKNOWN`.
#[tokio::test]
async fn a_version_that_vanishes_after_it_is_listed_is_looked_for_once_more() {
    let rig = Rig::new();
    let (bytes, digest) = manifests(&rig, 1).await.remove(0);
    assert_eq!(
        rig.tag("app", "latest", &bytes).await.status,
        StatusCode::CREATED
    );
    let gone = || Error::NotFound("version".to_owned());
    rig.faulty
        .fail_next_on(Op::Get, "oci/tags/app/latest/", gone());
    let once = rig.send(Method::GET, "/v2/app/manifests/latest").await;
    assert_eq!(once.status, StatusCode::OK, "{}", once.text());
    assert_eq!(once.header("docker-content-digest"), digest);
    rig.faulty
        .fail_next_on(Op::Get, "oci/tags/app/latest/", gone());
    rig.faulty
        .fail_next_on(Op::Get, "oci/tags/app/latest/", gone());
    let twice = rig.send(Method::GET, "/v2/app/manifests/latest").await;
    let (_, detail) = twice.assert_error(StatusCode::NOT_FOUND, "MANIFEST_UNKNOWN");
    assert_eq!(detail["tag"], "latest");
}

/// Asserts that a tag that is not there is 404 `MANIFEST_UNKNOWN` naming the tag, for GET and HEAD.
#[tokio::test]
async fn a_tag_that_is_not_there_is_404() {
    let rig = Rig::new();
    for method in [Method::GET, Method::HEAD] {
        let answer = rig.send(method, "/v2/app/manifests/missing").await;
        let (_, detail) = answer.assert_error(StatusCode::NOT_FOUND, "MANIFEST_UNKNOWN");
        assert_eq!(detail["tag"], "missing");
    }
    assert_eq!(rig.metrics.requests("manifest", "not_found"), 2);
}

/// Asserts that the list of a name is its tags in ascending order and no other name's (not the
/// tags of a name that has it as a prefix, nor of one inside it), is the same for HEAD with no
/// body, and is empty for a name with no tag.
#[tokio::test]
async fn the_list_of_a_name_is_its_own_tags_in_order() {
    let rig = Rig::new();
    let made = manifests(&rig, 2).await;
    let (first, second) = (made[0].0.clone(), made[1].0.clone());
    for tag in ["c", "a", "b"] {
        assert_eq!(
            rig.tag("app", tag, &first).await.status,
            StatusCode::CREATED
        );
    }
    // A tag that moved is listed once.
    assert_eq!(
        rig.tag("app", "a", &second).await.status,
        StatusCode::CREATED
    );
    assert_eq!(
        rig.tag("app/sub", "inner", &second).await.status,
        StatusCode::CREATED
    );
    assert_eq!(
        rig.tag("app2", "other", &second).await.status,
        StatusCode::CREATED
    );
    let list = rig.tags("app", "").await;
    assert_eq!(list.status, StatusCode::OK, "{}", list.text());
    assert_eq!(
        list.header("content-type"),
        "application/json; charset=utf-8"
    );
    let document: Value = serde_json::from_slice(&list.body).expect("a document");
    assert_eq!(document["name"], "app");
    assert_eq!(listed(&list), ["a", "b", "c"]);
    assert_eq!(listed(&rig.tags("app/sub", "").await), ["inner"]);
    assert_eq!(listed(&rig.tags("nobody", "").await), Vec::<String>::new());
    let head = rig.send(Method::HEAD, "/v2/app/tags/list").await;
    assert!(head.body.is_empty());
    assert_eq!(head.header("content-length"), list.body.len().to_string());
    assert_eq!(rig.metrics.requests("tags", "ok"), 4);
}

/// Asserts the paging of the list: `n` cuts a page and a `Link` names the next one (with the last
/// tag and the same `n`), `last` starts after a tag, a page that ends the list has no `Link`
/// (also when it is exactly `n` long), `n=0` is an empty page, and an `n` that is no number is 400
/// `PAGINATION_NUMBER_INVALID`.
#[tokio::test]
async fn the_list_is_paged_with_n_and_last() {
    let rig = Rig::new();
    let first = manifests(&rig, 1).await.remove(0).0;
    for tag in ["a", "b", "c"] {
        assert_eq!(
            rig.tag("app", tag, &first).await.status,
            StatusCode::CREATED
        );
    }
    let page = rig.tags("app", "?n=2").await;
    assert_eq!(listed(&page), ["a", "b"]);
    assert_eq!(
        page.header("link"),
        "</v2/app/tags/list?last=b&n=2>; rel=\"next\""
    );
    let next = rig.tags("app", "?last=b&n=2").await;
    assert_eq!(listed(&next), ["c"]);
    assert!(next.headers.get("link").is_none());
    let exact = rig.tags("app", "?n=3").await;
    assert_eq!(listed(&exact), ["a", "b", "c"]);
    assert!(exact.headers.get("link").is_none());
    let none = rig.tags("app", "?n=0").await;
    assert!(listed(&none).is_empty());
    assert!(none.headers.get("link").is_none());
    let after = rig.tags("app", "?last=a").await;
    assert_eq!(listed(&after), ["b", "c"]);
    let bad = rig.tags("app", "?n=many").await;
    let (_, detail) = bad.assert_error(StatusCode::BAD_REQUEST, "PAGINATION_NUMBER_INVALID");
    assert_eq!(detail["n"], "many");
}

/// Asserts that the list names each tag once however many versions it has, and in the order of
/// the tags, not of the keys (the keys of `a-b` and `a.c` sort before those of `a`, as `-` and
/// `.` sort before `/`), and that `n` and `last` page in that order: `last=a` starts at `a-b`.
#[tokio::test]
async fn the_tag_list_is_distinct_tags_in_tag_order_and_pages() {
    let rig = Rig::new();
    let made = manifests(&rig, 2).await;
    let (first, second) = (made[0].0.clone(), made[1].0.clone());
    for (tag, bytes) in [
        ("a", &first),
        ("a", &second),
        ("a-b", &first),
        ("a.c", &first),
        ("b", &first),
        ("b", &second),
    ] {
        assert_eq!(rig.tag("app", tag, bytes).await.status, StatusCode::CREATED);
    }
    assert_eq!(listed(&rig.tags("app", "").await), ["a", "a-b", "a.c", "b"]);
    assert_eq!(
        listed(&rig.tags("app", "?last=a").await),
        ["a-b", "a.c", "b"]
    );
    let page = rig.tags("app", "?n=2").await;
    assert_eq!(listed(&page), ["a", "a-b"]);
    assert_eq!(
        page.header("link"),
        "</v2/app/tags/list?last=a-b&n=2>; rel=\"next\""
    );
    let next = rig.tags("app", "?last=a-b&n=2").await;
    assert_eq!(listed(&next), ["a.c", "b"]);
    assert!(next.headers.get("link").is_none());
    assert_eq!(listed(&rig.tags("app", "?last=a.c").await), ["b"]);
}

/// Asserts that the list of a name that is not a valid name is 400 `NAME_INVALID`, and a store
/// that fails the listing is 503 `UNAVAILABLE`.
#[tokio::test]
async fn the_list_refuses_a_bad_name_and_a_failing_store() {
    let rig = Rig::new();
    let bad = rig.tags("Upper", "").await;
    bad.assert_error(StatusCode::BAD_REQUEST, "NAME_INVALID");
    rig.faulty.fail_next(Op::List, 1);
    let down = rig.tags("app", "").await;
    down.assert_error(StatusCode::SERVICE_UNAVAILABLE, "UNAVAILABLE");
    assert_eq!(rig.metrics.requests("tags", "invalid"), 1);
    assert_eq!(rig.metrics.requests("tags", "failed"), 1);
}

/// Asserts that `/v2/<name>/tags/list` is routed as the tags of the name, for a name with slashes
/// too, and that a path of another shape is not.
#[test]
fn the_path_of_the_list_is_routed_as_tags() {
    assert_eq!(route("/v2/a/tags/list"), Route::Tags { name: "a" });
    assert_eq!(route("/v2/a/b/tags/list"), Route::Tags { name: "a/b" });
    assert_eq!(route("/v2/tags/list"), Route::Other);
    assert_eq!(route("/v2/a/tags/lists"), Route::Other);
}
