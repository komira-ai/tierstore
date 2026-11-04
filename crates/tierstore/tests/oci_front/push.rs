//! A registry client pushes an image to the front over TLS, with the full key, through a proxy with
//! in-memory tiers: a layer in chunks, a config in one call, and a manifest by tag; and pulls it
//! back, byte for byte, from the same connection.

use std::sync::Arc;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use serde_json::json;
use tierstore::server::ServerConfig;
use tierstore::{Backend, SystemClock};

use crate::rig::{Rig, hex_of};

fn server() -> ServerConfig {
    ServerConfig::new("bucket", "FULL", "full-secret", Arc::new(SystemClock))
}

/// Asserts the pushes of a client that logs in with the full key, as the wire carries them: a layer
/// in two chunks and an end, a config in one `POST` with the digest percent-encoded as clients
/// send it, a manifest by tag; then the manifest and the layer are pulled back whole, the manifest
/// by its digest and the tag is not served, and no object of the uploads is left in the store.
#[tokio::test]
async fn a_client_pushes_an_image_in_chunks_and_pulls_it_back() {
    let rig = Rig::start_with_keys(&server()).await;
    let login = format!("Basic {}", STANDARD.encode("FULL:full-secret"));
    let auth = ("authorization", login.as_str());
    let mut conn = rig.conn().await;

    let layer = b"layer bytes, in two chunks";
    let layer_digest = format!("sha256:{}", hex_of(layer));
    let begun = conn
        .request("POST", "/v2/app/blobs/uploads/", &[auth])
        .await
        .expect("an answer");
    assert_eq!(begun.status, 202, "{begun:?}");
    let location = begun.must("location").to_owned();
    let (first, second) = layer.split_at(10);
    let chunk = conn
        .request_with_body("PATCH", &location, &[auth], first)
        .await
        .expect("an answer");
    assert_eq!((chunk.status, chunk.must("range")), (202, "0-9"));
    let range = format!("10-{}", layer.len() - 1);
    let headers = [auth, ("content-range", range.as_str())];
    let chunk = conn
        .request_with_body("PATCH", &location, &headers, second)
        .await
        .expect("an answer");
    let held = format!("0-{}", layer.len() - 1);
    assert_eq!((chunk.status, chunk.must("range")), (202, held.as_str()));
    let end = format!("{location}?digest={}", layer_digest.replace(':', "%3A"));
    let ended = conn
        .request_with_body("PUT", &end, &[auth], b"")
        .await
        .expect("an answer");
    assert_eq!(ended.status, 201, "{ended:?}");
    assert_eq!(ended.must("docker-content-digest"), layer_digest);

    let config = b"{}";
    let config_digest = format!("sha256:{}", hex_of(config));
    let target = format!(
        "/v2/app/blobs/uploads/?digest={}",
        config_digest.replace(':', "%3A")
    );
    let stored = conn
        .request_with_body("POST", &target, &[auth], config)
        .await
        .expect("an answer");
    assert_eq!(stored.status, 201, "{stored:?}");

    let manifest = json!({
        "schemaVersion": 2,
        "mediaType": "application/vnd.oci.image.manifest.v1+json",
        "config": { "digest": config_digest, "size": config.len() },
        "layers": [{ "digest": layer_digest, "size": layer.len() }],
    })
    .to_string();
    let manifest_digest = format!("sha256:{}", hex_of(manifest.as_bytes()));
    let pushed = conn
        .request_with_body(
            "PUT",
            "/v2/app/manifests/latest",
            &[auth],
            manifest.as_bytes(),
        )
        .await
        .expect("an answer");
    assert_eq!(pushed.status, 201, "{pushed:?}");
    assert_eq!(pushed.must("docker-content-digest"), manifest_digest);

    let pulled = conn
        .request(
            "GET",
            &format!("/v2/app/manifests/{manifest_digest}"),
            &[auth],
        )
        .await
        .expect("an answer");
    assert_eq!(
        (pulled.status, pulled.body.as_slice()),
        (200, manifest.as_bytes())
    );
    let blob = conn
        .request("GET", &format!("/v2/app/blobs/{layer_digest}"), &[auth])
        .await
        .expect("an answer");
    assert_eq!((blob.status, blob.body.as_slice()), (200, layer.as_slice()));
    let tag = conn
        .request("GET", "/v2/app/manifests/latest", &[auth])
        .await
        .expect("an answer");
    assert_eq!(tag.status, 404);

    let left = rig.proxy.list("oci/uploads/").await.expect("a listing");
    assert!(left.is_empty(), "{left:?}");
    assert_eq!(rig.metrics.requests("upload", "accepted"), 3);
    assert_eq!(rig.metrics.requests("upload", "created"), 2);
    assert_eq!(rig.metrics.requests("manifest", "created"), 1);
    rig.stop().await;
}

/// Asserts that bytes that are not the digest they are pushed with are refused 400 `DIGEST_INVALID`
/// over the wire and no blob is stored, and that a key that may only pull is refused 403.
#[tokio::test]
async fn a_wrong_digest_and_a_read_only_key_push_nothing() {
    let mut keys = server();
    keys.read_only_keys
        .push(tierstore::server::AccessKey::new("READER", "reader-secret"));
    let rig = Rig::start_with_keys(&keys).await;
    let full = format!("Basic {}", STANDARD.encode("FULL:full-secret"));
    let reader = format!("Basic {}", STANDARD.encode("READER:reader-secret"));
    let claimed = format!("sha256:{}", hex_of(b"promised"));
    let target = format!("/v2/app/blobs/uploads/?digest={claimed}");
    let mut conn = rig.conn().await;
    let refused = conn
        .request_with_body(
            "POST",
            &target,
            &[("authorization", full.as_str())],
            b"sent",
        )
        .await
        .expect("an answer");
    assert_eq!(refused.status, 400, "{refused:?}");
    assert_eq!(refused.error_code(), "DIGEST_INVALID");
    let denied = rig
        .conn()
        .await
        .request_with_body(
            "POST",
            &target,
            &[("authorization", reader.as_str())],
            b"promised",
        )
        .await
        .expect("an answer");
    assert_eq!(denied.status, 403, "{denied:?}");
    let left = rig.proxy.list("oci/").await.expect("a listing");
    assert!(left.is_empty(), "{left:?}");
    rig.stop().await;
}
