//! The names under which one digest is kept today.

use super::layout::name_of;
use super::pack_testing::{Pack, blob_hex};
use super::testing::{Rig, digest};

/// Asserts that the bytes of one digest are two cache entries today: the blob a mount asks for
/// through the fill socket is kept under `_tiering/fill/<sha256>` and the object of the same
/// bytes that an S3 read asks for under `cas/l/<sha256>`, each with its own file, its own name
/// and its own bytes on disk, so the upstream is read once for each and the cache holds the bytes
/// twice. This is the state the design of the prefetch call changes (one entry per digest); the
/// test names the change.
#[tokio::test]
async fn s3_object_and_mount_blob_of_one_digest_are_different_cache_entries() {
    let rig = Rig::new("tnames-two");
    let pack = Pack::put(&rig, "packs/names", &[900, 700]).await;
    let sha = pack.blobs[0].0.clone();
    let key = format!("cas/l/{sha}");
    rig.put(&key, pack.content(0)).await;

    rig.cache.fill_blob(&pack.blob(0)).await.unwrap();
    assert_eq!(rig.cache.held(), (900, 1));
    assert_eq!(rig.get(&key).await.unwrap(), pack.content(0));

    assert_eq!(rig.cache.held(), (1800, 2));
    assert_ne!(blob_hex(&sha), name_of(&key).hex);
    assert_eq!(name_of(&key).hex, sha);
    assert!(rig.has_file(&key));
    assert!(rig.has_file(&format!("_tiering/fill/{sha}")));
    assert_eq!(digest(pack.content(0)), sha);
}
