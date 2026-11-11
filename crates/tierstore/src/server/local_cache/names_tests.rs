//! The names under which one digest is kept: one entry for it, whichever path fills it, with the
//! origin of its bytes in the sidecar; and the entries of the previous layout, which are given
//! that name at start.

use std::os::unix::fs::PermissionsExt as _;

use super::layout::{Origin, Sidecar, data_path, meta_path, name_of, object_dir};
use super::metrics::read;
use super::pack_testing::{Pack, blob_hex};
use super::source::Blob;
use super::testing::{Rig, digest, pattern};
use crate::{Backend as _, Error, Op};
use bytes::Bytes;
use futures::StreamExt as _;
use futures::stream;

/// The blob `bytes` as a mount asks for it when it is the whole upstream object `cas/l/<sha256>`.
fn whole<'a>(sha: &'a str, key: &'a str, bytes: &[u8]) -> Blob<'a> {
    Blob {
        sha256: sha,
        size: bytes.len() as u64,
        from: key,
        offset: 0,
    }
}

/// Asserts that the bytes of one digest are one cache entry whichever path fills it: the blob a
/// mount asks for through the fill socket and the object that an S3 read asks for under
/// `cas/l/<sha256>` have one name and one file, one charge against the budget, and the upstream
/// is read once, whichever of the two comes first. (This replaces the pin-down test
/// `s3_object_and_mount_blob_of_one_digest_are_different_cache_entries`, which asserted the two
/// entries and the two reads that the design changes on purpose.)
#[tokio::test]
async fn one_digest_is_one_entry_whichever_path_fills_it() {
    let rig = Rig::new("tnames-one");
    let body = pattern(900);
    let sha = digest(&body);
    let key = format!("cas/l/{sha}");
    rig.put(&key, &body).await;

    rig.cache
        .fill_blob(&whole(&sha, &key, &body))
        .await
        .unwrap();
    assert_eq!(rig.cache.held(), (900, 1));
    let reads = rig.calls_on(Op::Get, &key);
    assert_eq!(rig.get(&key).await.unwrap(), body);
    assert_eq!(rig.cache.held(), (900, 1));
    assert_eq!(rig.calls_on(Op::Get, &key), reads);
    assert_eq!(blob_hex(&sha), name_of(&key).hex);
    assert_eq!(name_of(&key).hex, sha);
    assert_eq!(
        super::source::blob_path(&rig.config.path, &sha),
        rig.file(&key)
    );
    assert!(rig.has_file(&key));

    let other = pattern(700);
    let (osha, okey) = (digest(&other), format!("cas/l/{}", digest(&other)));
    rig.put(&okey, &other).await;
    assert_eq!(rig.get(&okey).await.unwrap(), other);
    let reads = rig.calls_on(Op::Get, &okey);
    rig.cache
        .fill_blob(&whole(&osha, &okey, &other))
        .await
        .unwrap();
    assert_eq!(rig.cache.held(), (1600, 2));
    assert_eq!(rig.calls_on(Op::Get, &okey), reads);
}

/// Asserts that an entry is of origin `span` when its bytes were cut out of a pack, and of origin
/// `object` when they are the whole upstream object of the key; and that an `object` sidecar is
/// the sidecar of the previous release byte for byte (the origin is left out), so that release,
/// run again on this directory, reads it as it reads any `cas/l` object, and ignores the field of
/// a `span`.
#[tokio::test]
async fn the_sidecar_says_the_origin_and_leaves_it_out_for_an_object() {
    let rig = Rig::new("tnames-origin");
    let pack = Pack::put(&rig, "packs/origin", &[300, 400]).await;
    rig.cache.fill_blob(&pack.blob(0)).await.unwrap();
    let body = pattern(500);
    let sha = digest(&body);
    let key = format!("cas/l/{sha}");
    rig.put(&key, &body).await;
    rig.cache
        .fill_blob(&whole(&sha, &key, &body))
        .await
        .unwrap();
    let read = |sha: &str| std::fs::read_to_string(meta_path(&rig.config.path, sha)).unwrap();
    let span = read(&pack.blobs[0].0);
    assert!(span.contains(r#""origin":"span""#), "{span}");
    let object = read(&sha);
    assert!(!object.contains("origin"), "{object}");

    // The previous release's sidecar: no `origin`, and it reads what is there.
    #[derive(serde::Deserialize)]
    struct Previous {
        key: String,
        size: u64,
        sha256: String,
    }
    let previous: Previous = serde_json::from_str(&span).unwrap();
    assert_eq!(
        (previous.key, previous.size, previous.sha256),
        (
            format!("cas/l/{}", pack.blobs[0].0),
            300,
            pack.blobs[0].0.clone()
        )
    );
    let sidecar: Sidecar = serde_json::from_str(&object).unwrap();
    assert_eq!(sidecar.origin, Origin::Object);
    assert_eq!(rig.reopen().held(), (800, 2));
}

/// Asserts that an entry of origin `span` is not an S3 object: a `GET`, a ranged `GET` and a
/// `HEAD` of `cas/l/<sha256>` go to the upstream and answer what it says (not found, when no
/// object has that key), and when the upstream does have the object the read is served from the
/// upstream and keeps nothing, leaving the one `span` entry as it was; an `object` entry is
/// served from the cache.
#[tokio::test]
async fn span_entry_is_not_an_s3_object() {
    let rig = Rig::new("tnames-span");
    let pack = Pack::put(&rig, "packs/span", &[900, 700]).await;
    let (sha, key) = (
        pack.blobs[0].0.clone(),
        format!("cas/l/{}", pack.blobs[0].0),
    );
    rig.cache.fill_blob(&pack.blob(0)).await.unwrap();
    assert_eq!(rig.cache.held(), (900, 1));

    let missing = Err(Error::NotFound(key.clone()));
    assert_eq!(rig.get(&key).await, missing);
    let range = crate::ByteRange::new(0, 10);
    assert_eq!(rig.cache.get(&key, Some(range)).await, missing);
    assert_eq!(
        rig.cache.head(&key).await.map(|meta| meta.size),
        Err(Error::NotFound(key.clone()))
    );
    assert_eq!(read(&rig.cache.counters().misses), 0);

    rig.put(&key, pack.content(0)).await;
    let reads = rig.calls_on(Op::Get, &key);
    assert_eq!(rig.get(&key).await.unwrap(), pack.content(0));
    assert_eq!(rig.calls_on(Op::Get, &key), reads + 1);
    assert_eq!(rig.cache.head(&key).await.map(|meta| meta.size), Ok(900));
    assert_eq!(rig.cache.held(), (900, 1));
    let kept = std::fs::read_to_string(meta_path(&rig.config.path, &sha)).unwrap();
    assert!(kept.contains(r#""origin":"span""#), "{kept}");

    let object = pattern(300);
    let okey = format!("cas/l/{}", digest(&object));
    rig.put(&okey, &object).await;
    assert_eq!(rig.get(&okey).await.unwrap(), object);
    let reads = rig.calls_on(Op::Get, &okey);
    assert_eq!(rig.get(&okey).await.unwrap(), object);
    assert_eq!(rig.calls_on(Op::Get, &okey), reads);
}

/// Writes an entry as the previous release kept it: the data file under the hash of the key
/// `_tiering/fill/<sha256>`, and a sidecar of that key with no `origin`. Gives the name.
fn write_previous(rig: &Rig, sha: &str, bytes: &[u8]) -> String {
    let old = name_of(&format!("_tiering/fill/{sha}")).hex;
    let root = &rig.config.path;
    std::fs::create_dir_all(object_dir(root, &old)).unwrap();
    std::fs::write(data_path(root, &old), bytes).unwrap();
    std::fs::set_permissions(
        data_path(root, &old),
        std::fs::Permissions::from_mode(0o444),
    )
    .unwrap();
    let sidecar = serde_json::json!({
        "key": format!("_tiering/fill/{sha}"),
        "size": bytes.len(),
        "sha256": sha,
        "tag": sha,
        "modified": 0,
    });
    std::fs::write(meta_path(root, &old), sidecar.to_string()).unwrap();
    old
}

/// Asserts that the entries of the previous layout are renamed to their digests at start: the
/// data file and the sidecar are at the digest's name and the old names are gone; the sidecar
/// reads as an entry of origin `span` under `cas/l/<sha256>`; the entry is in the index with its
/// size, a fill socket request finds it present and asks the upstream nothing, a pin that the
/// previous release saved under the old name is restored under the new one (and saved under it),
/// and the cache of the next start reads the same.
#[tokio::test]
async fn entries_of_the_previous_layout_are_renamed_at_start() {
    let rig = Rig::with("tnames-rename", |config| config.pin_limit = 1 << 20);
    let pack = Pack::new("packs/old", &[600, 800]);
    let sha = pack.blobs[1].0.clone();
    let old = write_previous(&rig, &sha, pack.content(1));
    let pins = serde_json::json!({"trees": [{
        "mount": "m1", "tree": "t1", "blobs": [old.clone()],
    }]});
    std::fs::write(rig.config.path.join("pins"), pins.to_string()).unwrap();

    for _ in 0..2 {
        let cache = rig.reopen();
        let root = &rig.config.path;
        assert_eq!(cache.held(), (800, 1));
        assert!(!data_path(root, &old).exists());
        assert!(!meta_path(root, &old).exists());
        assert_eq!(
            std::fs::read(data_path(root, &sha)).unwrap(),
            pack.content(1)
        );
        let text = std::fs::read(meta_path(root, &sha)).unwrap();
        let sidecar: Sidecar = serde_json::from_slice(&text).unwrap();
        assert_eq!(sidecar.key, format!("cas/l/{sha}"));
        assert_eq!(sidecar.origin, Origin::Span);
        assert_eq!(cache.pinned_bytes(), 800);
        let blobs = [sha.clone()];
        let answer = cache.pin("m1", "t1", &blobs).unwrap();
        assert_eq!((answer.pinned, answer.absent), (1, 0));
        assert_eq!(rig.calls(Op::Get), 0);
        // The pins file now names the digest, as the next start reads it.
        cache.inner.save_pins(true);
    }
}

/// Asserts that when the digest's name is taken at start (the same bytes were kept under it as an
/// object) the entry of the previous layout is removed and the one at the digest's name stays,
/// as it was, of origin `object`; and that an entry of the previous layout whose rename cannot be
/// made (the directory of the digest's name is a file) is removed and costs only a refill.
#[tokio::test]
async fn an_old_entry_is_removed_when_its_digest_name_is_taken_or_cannot_be_made() {
    let rig = Rig::with("tnames-taken", |config| config.pin_limit = 1 << 20);
    let pack = Pack::new("packs/taken", &[600, 800, 500]);
    let (first, blocked) = (pack.blobs[0].0.clone(), pack.blobs[2].0.clone());
    let key = format!("cas/l/{first}");
    rig.put(&key, pack.content(0)).await;
    assert_eq!(rig.get(&key).await.unwrap(), pack.content(0));
    let old = write_previous(&rig, &first, pack.content(0));
    let stuck = write_previous(&rig, &blocked, pack.content(2));
    let pins = serde_json::json!({"trees": [{
        "mount": "m1", "tree": "t1", "blobs": [old.clone()],
    }]});
    std::fs::write(rig.config.path.join("pins"), pins.to_string()).unwrap();
    let root = &rig.config.path;
    let digest_dir = object_dir(root, &blocked);
    std::fs::create_dir_all(digest_dir.parent().unwrap()).unwrap();
    std::fs::write(&digest_dir, b"in the way").unwrap();

    let cache = rig.reopen();
    assert_eq!(cache.held(), (600, 1));
    // The pin of the removed old entry is on the entry at the digest's name.
    assert_eq!(cache.pinned_bytes(), 600);
    for gone in [&old, &stuck] {
        assert!(!data_path(root, gone).exists());
        assert!(!meta_path(root, gone).exists());
    }
    let text = std::fs::read_to_string(meta_path(root, &first)).unwrap();
    assert!(!text.contains("origin"), "{text}");
    assert_eq!(
        std::fs::read(data_path(root, &first)).unwrap(),
        pack.content(0)
    );
}

/// Pins the blob `i` of `pack` as a `span` entry for a tree of a mount, and gives its digest.
async fn pinned_span(rig: &Rig, pack: &Pack, i: usize) -> String {
    let sha = pack.blobs[i].0.clone();
    rig.cache.fill_blob(&pack.blob(i)).await.unwrap();
    let answer = rig
        .cache
        .pin("m1", "t1", std::slice::from_ref(&sha))
        .unwrap();
    assert_eq!((answer.pinned, answer.absent), (1, 0));
    sha
}

/// Asserts that a client's put of `cas/l/<sha256>` over a blob of the same digest that a mount
/// pinned keeps the pin: the entry becomes an object (the sidecar loses its `origin`) and the
/// tree still pins its bytes, so eviction cannot take it from under the job.
#[tokio::test]
async fn a_put_of_the_digest_of_a_pinned_span_keeps_the_pin() {
    let rig = Rig::with("tnames-put-pinned", |config| {
        config.write_through = true;
        config.pin_limit = 1 << 20;
    });
    let pack = Pack::put(&rig, "packs/put", &[900, 700]).await;
    let sha = pinned_span(&rig, &pack, 0).await;
    let key = format!("cas/l/{sha}");
    let body = Bytes::copy_from_slice(pack.content(0));
    let sent = stream::once(async move { Ok(body) }).boxed();
    assert_eq!(rig.cache.put_stream(&key, Some(900), sent).await, Ok(()));
    let meta = meta_path(&rig.config.path, &sha);
    for _ in 0..1_000_000 {
        let text = std::fs::read_to_string(&meta).unwrap_or_default();
        if !text.is_empty() && !text.contains("origin") {
            break;
        }
        tokio::task::yield_now().await;
    }
    let text = std::fs::read_to_string(&meta).unwrap();
    assert!(!text.contains("origin"), "{text}");
    assert_eq!(rig.cache.pinned_bytes(), 900);
}

/// Asserts that a delete of `cas/l/<sha256>` leaves a pinned blob of that digest that is only a
/// `span` where it is: no object of the upstream was deleted.
#[tokio::test]
async fn a_delete_of_the_digest_of_a_pinned_span_leaves_it() {
    let rig = Rig::with("tnames-del-pinned", |config| config.pin_limit = 1 << 20);
    let pack = Pack::put(&rig, "packs/del", &[900, 700]).await;
    let sha = pinned_span(&rig, &pack, 0).await;
    rig.cache.inner.forget(&format!("cas/l/{sha}"));
    assert_eq!(rig.cache.held(), (900, 1));
    assert_eq!(rig.cache.pinned_bytes(), 900);
    assert!(rig.has_file(&format!("cas/l/{sha}")));
}
