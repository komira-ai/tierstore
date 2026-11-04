//! Tags of the OCI front: pushed, pulled, listed and moved, over an in-memory store, with no
//! socket. Two pushes of one tag are made to meet at the store with a barrier, not with time.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use async_trait::async_trait;
use bytes::Bytes;
use hyper::{Method, StatusCode};
use serde_json::Value;
use tokio::sync::Barrier;

use super::names::{Route, route};
use super::push_manifest_tests::{image, stocked};
use super::push_tests::{Answer, Rig, digest_of, front_over, full};
use crate::{Backend, ByteRange, Error, FaultyBackend, Listed, MemoryBackend, Meta, Op};

/// A store whose reads of a tag, once armed, wait for a second reader before they answer, so that
/// two pushes of one tag have both read its state before either writes.
struct Gated {
    inner: Arc<dyn Backend>,
    barrier: Arc<Barrier>,
    armed: AtomicBool,
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
        let read = self.inner.head(key).await;
        if key.contains("/tags/") && self.armed.load(Ordering::SeqCst) {
            self.barrier.wait().await;
        }
        read
    }

    async fn delete(&self, key: &str) -> Result<(), Error> {
        self.inner.delete(key).await
    }

    async fn list(&self, prefix: &str) -> Result<Vec<Listed>, Error> {
        self.inner.list(prefix).await
    }
}

/// A rig over a gate, and the gate.
fn gated() -> (Rig, Arc<Gated>) {
    let memory = Arc::new(MemoryBackend::new());
    let faulty = Arc::new(FaultyBackend::new(memory.clone()));
    let gate = Arc::new(Gated {
        inner: faulty.clone(),
        barrier: Arc::new(Barrier::new(2)),
        armed: AtomicBool::new(false),
    });
    let (front, metrics) = front_over(gate.clone());
    let rig = Rig {
        faulty,
        memory,
        front,
        metrics,
    };
    (rig, gate)
}

impl Rig {
    /// Pushes `bytes` under `tag` of `name`.
    async fn tag(&self, name: &str, tag: &str, bytes: &[u8]) -> Answer {
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
async fn manifests(rig: &Rig, count: usize) -> Vec<(Vec<u8>, String)> {
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
/// media type and digest, that the tag is one object under the reserved directory of its name
/// (a name with slashes written with `~`), and that a push by digest makes no tag.
#[tokio::test]
async fn a_pushed_tag_is_pulled_by_the_tag() {
    let (rig, config, layer) = stocked().await;
    let bytes = image(&config, &[&layer]);
    let digest = digest_of(&bytes);
    let put = rig.tag("lib/app", "v1", &bytes).await;
    assert_eq!(put.status, StatusCode::CREATED, "{}", put.text());
    assert_eq!(put.header("docker-content-digest"), digest);
    let held = rig.memory.get("oci/tags/lib~app/v1", None).await;
    assert_eq!(held.expect("a tag object"), digest.as_bytes());
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
    assert_eq!(
        rig.tag("app", "latest", &second).await.status,
        StatusCode::CREATED
    );
}

/// Asserts that of two pushes of a tag that does not exist, which both found it missing before
/// either wrote, exactly one is 201 and the tag points at its manifest, and the other is 409
/// `TAG_CONFLICT` and leaves the tag alone.
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
    gate.armed.store(false, Ordering::SeqCst);
    let held = rig.memory.get("oci/tags/app/latest", None).await;
    assert_eq!(held.expect("a tag"), winners[0].1.as_bytes());
    assert_eq!(rig.metrics.requests("manifest", "created"), 1);
    assert_eq!(rig.metrics.requests("manifest", "conflict"), 1);
}

/// Asserts that of two pushes of a tag that exists, which both read its state before either
/// wrote, exactly one is 201 and the other is 409; the manifest of the loser is stored by its
/// digest, the tag points at the winner's, and a push of the loser's manifest after that is taken.
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
    let held = rig.memory.get("oci/tags/app/latest", None).await;
    assert_eq!(held.expect("a tag"), winner.as_bytes());
    assert_ne!(first_digest, winner);
    let again = rig.tag("app", "latest", loser).await;
    assert_eq!(again.status, StatusCode::CREATED, "{}", again.text());
    let by_tag = rig.send(Method::GET, "/v2/app/manifests/latest").await;
    assert_eq!(&by_tag.body, loser);
}

/// Asserts that a tag that was deleted between the push's read and its write is a lost push,
/// 409, and not a failure of the store.
#[tokio::test]
async fn a_tag_deleted_under_a_push_is_a_conflict() {
    let rig = Rig::new();
    let made = manifests(&rig, 2).await;
    let (first, second) = (made[0].0.clone(), made[1].0.clone());
    assert_eq!(
        rig.tag("app", "latest", &first).await.status,
        StatusCode::CREATED
    );
    let gone = Error::NotFound("oci/tags/app/latest".to_owned());
    rig.faulty.fail_next_with(Op::PutIfMatch, gone);
    let put = rig.tag("app", "latest", &second).await;
    put.assert_error(StatusCode::CONFLICT, "TAG_CONFLICT");
}

/// Asserts that a store that fails while a tag is read or written is 503 `UNAVAILABLE`, on the
/// read of the tag's state, on its create and on its replacement, and that a tag object that is
/// not a digest is 500 `UNKNOWN` and is not served.
#[tokio::test]
async fn a_store_that_fails_under_a_tag_is_503() {
    let rig = Rig::new();
    let made = manifests(&rig, 2).await;
    let (first, second) = (made[0].0.clone(), made[1].0.clone());
    let down = Error::Unavailable("down".to_owned());
    rig.faulty.fail_next_on(Op::Head, "oci/tags/", down.clone());
    let read = rig.tag("app", "latest", &first).await;
    read.assert_error(StatusCode::SERVICE_UNAVAILABLE, "UNAVAILABLE");
    assert_eq!(
        rig.tag("app", "latest", &first).await.status,
        StatusCode::CREATED
    );
    rig.faulty.fail_next_with(Op::PutIfMatch, down.clone());
    let replace = rig.tag("app", "latest", &second).await;
    replace.assert_error(StatusCode::SERVICE_UNAVAILABLE, "UNAVAILABLE");
    rig.faulty.fail_next_on(Op::Put, "oci/tags/", down);
    let create = rig.tag("app", "other", &first).await;
    create.assert_error(StatusCode::SERVICE_UNAVAILABLE, "UNAVAILABLE");
    let bad = rig
        .memory
        .put("oci/tags/app/bad", Bytes::from_static(b"nope"))
        .await;
    bad.expect("put");
    let served = rig.send(Method::GET, "/v2/app/manifests/bad").await;
    served.assert_error(StatusCode::INTERNAL_SERVER_ERROR, "UNKNOWN");
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
