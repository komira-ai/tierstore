//! What the OCI front writes and deletes, seen from the store: the contract the proxy's put
//! protocol relies on for every push (`docs/design/put-without-lock.md`, 5.17).

use hyper::{Method, StatusCode};

use super::push_manifest_tests::{image, stocked};
use super::push_tests::{Rig, digest_of, full};
use crate::Op;

/// Asserts that a push of every kind (a monolithic blob, a chunked blob, a manifest by digest,
/// a tag, the same tag moved, and a cancelled upload) calls only the store's create-only writes:
/// it never replaces an object by its tag and never deletes one by its tag, and the only objects
/// it deletes are the chunk and session objects of an upload, under `oci/uploads/`. A front that
/// replaced an object, or deleted a blob, a manifest or a tag version, would fail it.
#[tokio::test]
async fn a_push_of_every_kind_writes_create_only_and_deletes_only_upload_objects() {
    let (rig, config, layer) = stocked().await;

    let monolithic = digest_of(b"monolithic");
    let target = format!("/v2/app/blobs/uploads/?digest={monolithic}");
    let sent = rig
        .ask(Method::POST, &target, &[], full(b"monolithic"))
        .await;
    assert_eq!(sent.status, StatusCode::CREATED, "{}", sent.text());

    let id = rig.begin().await;
    let session = Rig::session(&id);
    rig.ask(Method::PATCH, &session, &[], full(b"chunk")).await;
    let chunked = digest_of(b"chunk");
    let end = format!("{session}?digest={chunked}");
    let done = rig.send(Method::PUT, &end).await;
    assert_eq!(done.status, StatusCode::CREATED, "{}", done.text());

    let manifest = image(&config, &[&layer]);
    let by_digest = format!("/v2/app/manifests/{}", digest_of(&manifest));
    for target in [
        by_digest.as_str(),
        "/v2/app/manifests/v1",
        "/v2/app/manifests/v1",
    ] {
        let pushed = rig.ask(Method::PUT, target, &[], full(&manifest)).await;
        assert_eq!(
            pushed.status,
            StatusCode::CREATED,
            "{target}: {}",
            pushed.text()
        );
    }

    let cancelled = rig.begin().await;
    let session = Rig::session(&cancelled);
    rig.ask(Method::PATCH, &session, &[], full(b"abc")).await;
    let gone = rig.send(Method::DELETE, &session).await;
    assert_eq!(gone.status, StatusCode::NO_CONTENT, "{}", gone.text());

    assert_eq!(rig.faulty.calls(Op::PutIfMatch), 0);
    assert_eq!(rig.faulty.calls(Op::DeleteIfMatch), 0);
    let deleted: Vec<String> = rig
        .faulty
        .call_log()
        .into_iter()
        .filter(|(op, _)| *op == Op::Delete)
        .map(|(_, key)| key)
        .collect();
    // The chunk and the session object of the finished upload, and of the cancelled one.
    assert_eq!(deleted.len(), 4, "{deleted:?}");
    for key in deleted {
        assert!(key.starts_with("oci/uploads/"), "{key}");
    }
    assert!(rig.staged().await.is_empty());
}
