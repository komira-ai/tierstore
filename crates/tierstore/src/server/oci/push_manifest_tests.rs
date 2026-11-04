//! `PUT` of a manifest to the OCI front, and the digests a manifest names, over an in-memory store,
//! with no socket.

use hyper::{Method, StatusCode};
use serde_json::{Value, json};

use super::MAX_MANIFEST_BYTES;
use super::manifest::references;
use super::push_tests::{Rig, digest_of, full, scripted};
use crate::server::testing::Step;
use crate::{Backend, Op};

/// An image manifest naming `config` and `layers`.
fn image<S: AsRef<str>>(config: &str, layers: &[S]) -> Vec<u8> {
    let layers: Vec<Value> = layers
        .iter()
        .map(|digest| json!({ "digest": digest.as_ref() }))
        .collect();
    let manifest = json!({
        "schemaVersion": 2,
        "mediaType": "application/vnd.oci.image.manifest.v1+json",
        "config": { "digest": config },
        "layers": layers,
    });
    manifest.to_string().into_bytes()
}

/// An index naming `manifests`.
fn index<S: AsRef<str>>(manifests: &[S]) -> Vec<u8> {
    let manifests: Vec<Value> = manifests
        .iter()
        .map(|digest| json!({ "digest": digest.as_ref() }))
        .collect();
    let index = json!({ "schemaVersion": 2, "manifests": manifests });
    index.to_string().into_bytes()
}

/// A rig with a config and a layer stored, and their digests.
async fn stocked() -> (Rig, String, String) {
    let rig = Rig::new();
    let config = rig.seed(b"{}").await;
    let layer = rig.seed(b"layer bytes").await;
    (rig, config, layer)
}

/// Asserts that a manifest whose blobs are stored is stored by its digest: 201, its `Location` and
/// `Docker-Content-Digest`, and the front serves it back byte for byte with its media type.
#[tokio::test]
async fn a_manifest_is_stored_by_its_digest_and_served_back() {
    let (rig, config, layer) = stocked().await;
    let bytes = image(&config, &[&layer]);
    let digest = digest_of(&bytes);
    let target = format!("/v2/app/manifests/{digest}");
    let put = rig.ask(Method::PUT, &target, &[], full(&bytes)).await;
    assert_eq!(put.status, StatusCode::CREATED, "{}", put.text());
    assert_eq!(put.header("location"), target);
    assert_eq!(put.header("docker-content-digest"), digest);
    assert_eq!(put.header("content-length"), "0");
    let got = rig.send(Method::GET, &target).await;
    assert_eq!(got.body, bytes);
    assert_eq!(
        got.header("content-type"),
        "application/vnd.oci.image.manifest.v1+json"
    );
    assert_eq!(rig.metrics.requests("manifest", "created"), 1);
}

/// Asserts that a tag as the reference is accepted and not kept: 201 with the digest, the manifest
/// is served by that digest, and the tag is 404 `MANIFEST_UNKNOWN` as it always was.
#[tokio::test]
async fn a_tag_is_accepted_and_not_kept() {
    let (rig, config, layer) = stocked().await;
    let bytes = image(&config, &[&layer]);
    let digest = digest_of(&bytes);
    let put = rig
        .ask(Method::PUT, "/v2/app/manifests/latest", &[], full(&bytes))
        .await;
    assert_eq!(put.status, StatusCode::CREATED, "{}", put.text());
    assert_eq!(
        put.header("location"),
        format!("/v2/app/manifests/{digest}")
    );
    assert_eq!(put.header("docker-content-digest"), digest);
    let by_digest = rig
        .send(Method::GET, &format!("/v2/app/manifests/{digest}"))
        .await;
    assert_eq!(by_digest.body, bytes);
    let by_tag = rig.send(Method::GET, "/v2/app/manifests/latest").await;
    by_tag.assert_error(StatusCode::NOT_FOUND, "MANIFEST_UNKNOWN");
}

/// Asserts that a manifest that names a blob the front does not have is refused 400
/// `MANIFEST_BLOB_UNKNOWN`, naming the digest, whether it is the config, a layer or (in an index)
/// a manifest, and that nothing is stored; once the blob is pushed the same manifest is stored.
#[tokio::test]
async fn a_manifest_that_names_a_missing_blob_is_refused() {
    let (rig, config, layer) = stocked().await;
    let absent = digest_of(b"absent");
    for bytes in [
        image(&absent, &[&layer]),
        image(&config, &[&layer, &absent]),
        index(&[&absent]),
    ] {
        let digest = digest_of(&bytes);
        let target = format!("/v2/app/manifests/{digest}");
        let put = rig.ask(Method::PUT, &target, &[], full(&bytes)).await;
        let (_, detail) = put.assert_error(StatusCode::BAD_REQUEST, "MANIFEST_BLOB_UNKNOWN");
        assert_eq!(detail["digest"], absent);
        assert!(rig.blob(&digest).await.is_none());
    }
    let bytes = image(&config, &[&layer, &absent]);
    rig.seed(b"absent").await;
    let target = format!("/v2/app/manifests/{}", digest_of(&bytes));
    let put = rig.ask(Method::PUT, &target, &[], full(&bytes)).await;
    assert_eq!(put.status, StatusCode::CREATED, "{}", put.text());
}

/// Asserts that an index whose manifests are stored is stored.
#[tokio::test]
async fn an_index_of_stored_manifests_is_stored() {
    let (rig, config, layer) = stocked().await;
    let manifest = rig.seed(&image(&config, &[&layer])).await;
    let bytes = index(&[&manifest]);
    let digest = digest_of(&bytes);
    let target = format!("/v2/app/manifests/{digest}");
    let put = rig.ask(Method::PUT, &target, &[], full(&bytes)).await;
    assert_eq!(put.status, StatusCode::CREATED, "{}", put.text());
    let got = rig.send(Method::GET, &target).await;
    assert_eq!(
        got.header("content-type"),
        "application/vnd.oci.image.index.v1+json"
    );
}

/// Asserts that a manifest that does not hash to the digest it is put by is 400 `DIGEST_INVALID`
/// naming both digests, and is not stored.
#[tokio::test]
async fn a_manifest_that_is_not_its_reference_is_refused() {
    let (rig, config, layer) = stocked().await;
    let bytes = image(&config, &[&layer]);
    let wrong = digest_of(b"something else");
    let target = format!("/v2/app/manifests/{wrong}");
    let put = rig.ask(Method::PUT, &target, &[], full(&bytes)).await;
    let (_, detail) = put.assert_error(StatusCode::BAD_REQUEST, "DIGEST_INVALID");
    assert_eq!(detail["digest"], wrong);
    assert_eq!(detail["found"], digest_of(&bytes));
    assert!(rig.blob(&digest_of(&bytes)).await.is_none());
    assert!(rig.blob(&wrong).await.is_none());
}

/// Asserts that a reference that is neither a digest nor a tag is 400 `DIGEST_INVALID`.
#[tokio::test]
async fn a_reference_that_is_neither_a_digest_nor_a_tag_is_refused() {
    let (rig, config, layer) = stocked().await;
    let bytes = image(&config, &[&layer]);
    for reference in ["sha256:xyz", "-bad", "a%20b"] {
        let target = format!("/v2/app/manifests/{reference}");
        let put = rig.ask(Method::PUT, &target, &[], full(&bytes)).await;
        put.assert_error(StatusCode::BAD_REQUEST, "DIGEST_INVALID");
    }
}

/// Asserts that a body that is not a manifest is 400 `MANIFEST_INVALID` (text that is not JSON, JSON
/// that is not a manifest), and that a manifest that names a digest that is not sha256 is too.
#[tokio::test]
async fn a_body_that_is_not_a_manifest_is_refused() {
    let (rig, config, _) = stocked().await;
    let sha512 = image(&config, &["sha512:abcdef"]);
    for bytes in [b"not json".to_vec(), br#"{"a":1}"#.to_vec(), sha512] {
        let put = rig
            .ask(Method::PUT, "/v2/app/manifests/latest", &[], full(&bytes))
            .await;
        put.assert_error(StatusCode::BAD_REQUEST, "MANIFEST_INVALID");
    }
    assert_eq!(rig.metrics.requests("manifest", "invalid"), 3);
}

/// Asserts that a manifest over the limit of a manifest is refused 413 `MANIFEST_INVALID`, and a
/// manifest body that is cut is 400.
#[tokio::test]
async fn a_manifest_over_the_limit_or_cut_is_refused() {
    let rig = Rig::new();
    let limit = usize::try_from(MAX_MANIFEST_BYTES).expect("a size");
    let big = vec![b' '; limit + 1];
    let put = rig
        .ask(Method::PUT, "/v2/app/manifests/latest", &[], full(&big))
        .await;
    let (message, _) = put.assert_error(StatusCode::PAYLOAD_TOO_LARGE, "MANIFEST_INVALID");
    assert!(message.contains("over the limit"), "{message}");
    let steps = vec![Step::Data(b"{"), Step::Fail];
    let cut = rig
        .ask(
            Method::PUT,
            "/v2/app/manifests/latest",
            &[],
            scripted(steps),
        )
        .await;
    cut.assert_error(StatusCode::BAD_REQUEST, "MANIFEST_INVALID");
    assert!(rig.objects().await.is_empty());
}

/// Asserts that a manifest that is put twice is created twice, and stays one object.
#[tokio::test]
async fn a_manifest_put_twice_is_created_twice() {
    let (rig, config, layer) = stocked().await;
    let bytes = image(&config, &[&layer]);
    for _ in 0..2 {
        let put = rig
            .ask(Method::PUT, "/v2/app/manifests/latest", &[], full(&bytes))
            .await;
        assert_eq!(put.status, StatusCode::CREATED, "{}", put.text());
    }
    assert_eq!(rig.objects().await.len(), 3);
}

/// Asserts that a store that fails while the blobs of a manifest are looked up, or while it is
/// stored, is 503 `UNAVAILABLE`, and a manifest is stored by neither.
#[tokio::test]
async fn a_store_that_fails_is_503_and_stores_no_manifest() {
    let (rig, config, layer) = stocked().await;
    let bytes = image(&config, &[&layer]);
    rig.faulty.fail_next(Op::Head, 1);
    let put = rig
        .ask(Method::PUT, "/v2/app/manifests/latest", &[], full(&bytes))
        .await;
    put.assert_error(StatusCode::SERVICE_UNAVAILABLE, "UNAVAILABLE");
    rig.faulty.fail_next(Op::Put, 1);
    let put = rig
        .ask(Method::PUT, "/v2/app/manifests/latest", &[], full(&bytes))
        .await;
    put.assert_error(StatusCode::SERVICE_UNAVAILABLE, "UNAVAILABLE");
    let digest = digest_of(&bytes);
    assert!(rig.blob(&digest).await.is_none());
    let kept = rig.memory.list("oci/blobs/").await.expect("a listing");
    assert_eq!(kept.len(), 2);
}

/// Asserts the digests a manifest names: the config, the layers and the manifests, in that order;
/// an entry with no digest, a body that is not JSON and a manifest that names nothing give none.
#[test]
fn the_digests_a_manifest_names_are_its_config_layers_and_manifests() {
    let both = br#"{"config":{"digest":"c"},"layers":[{"digest":"l1"},{"size":1},{"digest":"l2"}],"manifests":[{"digest":"m"}]}"#;
    assert_eq!(references(both), ["c", "l1", "l2", "m"]);
    assert!(references(b"not json").is_empty());
    assert!(references(br#"{"config":{},"layers":[]}"#).is_empty());
    assert!(references(br#"{"layers":"x","config":{"digest":1}}"#).is_empty());
}
