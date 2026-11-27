//! A single PUT whose body fails, against the loopback S3 service of `s3::fake` made to keep a PUT
//! cut short as SeaweedFS 4.54 can: what the store is left with under the key.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "an integration test: a failed unwrap or expect is the failure it reports"
)]

use bytes::Bytes;
use futures::StreamExt as _;
use objstore_s3::s3::fake::FakeS3;
use objstore_s3::{BodyStream, Capabilities, ObjectKey, ObjectStore as _, ObjectStoreError};

const CONDITIONAL: Capabilities = Capabilities {
    conditional_put: true,
    object_lock: false,
};

fn key(name: &str) -> ObjectKey {
    ObjectKey::new(name).unwrap()
}

/// A body that gives `pieces` and then fails, as the client of an upload that went away: each
/// item waits a turn of the runtime first, so the HTTP client sends what it has (the head, and
/// every piece but the one it holds back) before the failure ends the request.
fn cut(pieces: &[&'static [u8]]) -> BodyStream {
    let mut items: Vec<Result<Bytes, ObjectStoreError>> = pieces
        .iter()
        .map(|piece| Ok(Bytes::from_static(piece)))
        .collect();
    items.push(Err(ObjectStoreError::Body("the client went away".into())));
    futures::stream::iter(items)
        .then(|item| async move {
            tokio::task::yield_now().await;
            item
        })
        .boxed()
}

/// Asserts that a create-only PUT whose body fails before its first byte, on a store that keeps it
/// as an object of 0 bytes, leaves no object under the key.
#[tokio::test]
async fn a_put_cut_before_its_first_byte_leaves_no_object() {
    let s3 = FakeS3::start().await;
    s3.keep_cut_bodies();
    s3.hold_heads();
    let store = s3.store(CONDITIONAL);
    let k = key("cas/cut-empty");
    let put = tokio::spawn({
        let k = k.clone();
        async move { store.put_stream(&k, 10, cut(&[])).await }
    });
    s3.wait_for(|s| s.cut_requests() == 1).await;
    s3.release_heads();
    let failed = put.await.unwrap().unwrap_err();
    assert!(matches!(failed, ObjectStoreError::Body(_)), "{failed}");
    assert_eq!(s3.object_len(&k), None);
}
