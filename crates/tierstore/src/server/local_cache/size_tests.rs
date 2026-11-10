//! The cache does not sync its fills, so a power cut may leave a file that is not the size the
//! cache recorded for it. Every open checks the size, and a file that is not that size is never
//! served: the entry is dropped and the object is fetched again.

use std::io::Write as _;
use std::os::unix::fs::PermissionsExt as _;

use bytes::Bytes;
use futures::StreamExt as _;
use futures::stream;

use super::pack_testing::Pack;
use super::source::blob_path;
use super::testing::{Rig, digest, pattern};
use crate::{Backend as _, ByteRange, Op};

/// Makes the data file at `path` writable, as the test needs to damage it.
fn open_up(path: &std::path::Path) {
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o644)).unwrap();
}

/// Cuts the file at `path` to `len` bytes, as a power cut leaves a file whose end never reached
/// the disk.
fn cut(path: &std::path::Path, len: u64) {
    open_up(path);
    std::fs::OpenOptions::new()
        .write(true)
        .open(path)
        .unwrap()
        .set_len(len)
        .unwrap();
}

/// Asserts that a fill request for a manifest, which the filesystem process answers by opening
/// the cache file as it is, is not answered `ok` for a file that was cut short or has grown: the
/// manifest is fetched again (a second GET) and the file holds the whole object. The file is
/// whole at the start, so the request before the damage is a hit and asks nothing.
#[tokio::test]
async fn a_fill_request_for_a_manifest_of_the_wrong_size_fetches_it_again() {
    let rig = Rig::new("tsize-manifest");
    let body = pattern(3000);
    let sha = digest(&body);
    let key = format!("cas/l/{sha}");
    rig.put(&key, &body).await;
    rig.cache.fill_manifest(&sha).await.unwrap();
    rig.cache.fill_manifest(&sha).await.unwrap();
    assert_eq!(rig.calls(Op::Get), 1);

    cut(&rig.file(&key), 1200);
    rig.cache.fill_manifest(&sha).await.unwrap();
    assert_eq!(rig.calls(Op::Get), 2);
    assert_eq!(std::fs::read(rig.file(&key)).unwrap(), body);

    open_up(&rig.file(&key));
    std::fs::OpenOptions::new()
        .append(true)
        .open(rig.file(&key))
        .unwrap()
        .write_all(b"more")
        .unwrap();
    rig.cache.fill_manifest(&sha).await.unwrap();
    assert_eq!(rig.calls(Op::Get), 3);
    assert_eq!(std::fs::read(rig.file(&key)).unwrap(), body);
}

/// Asserts the same for a blob inside a larger object: a file cut to a size that is not the
/// blob's is fetched again by the next fill request, and what is kept is the blob's bytes.
#[tokio::test]
async fn a_fill_request_for_a_blob_of_the_wrong_size_fetches_it_again() {
    let rig = Rig::new("tsize-blob");
    let pack = Pack::put(&rig, "packs/size", &[800, 2000]).await;
    rig.cache.fill_blob(&pack.blob(1)).await.unwrap();
    let file = blob_path(&rig.config.path, &pack.blobs[1].0);
    cut(&file, 0);
    rig.cache.fill_blob(&pack.blob(1)).await.unwrap();
    assert_eq!(rig.calls(Op::Get), 2);
    assert_eq!(std::fs::read(&file).unwrap(), pack.content(1));
}

/// Asserts that a read is not served from a file whose size is not the recorded one, even when
/// the bytes asked for lie inside the file: a range at the start of a file cut to 400 bytes, and
/// a whole read of a file that has grown, are each answered from the upstream with the right
/// bytes, and the file is whole after. Before the check a read was refused only when it reached
/// past the end of the file.
#[tokio::test]
async fn a_read_of_a_file_of_the_wrong_size_is_a_miss_that_fills_it_again() {
    let rig = Rig::new("tsize-read");
    let body = pattern(1000);
    rig.put("cas/b/obj", &body).await;
    rig.get("cas/b/obj").await.unwrap();
    let gets = rig.calls(Op::Get);

    cut(&rig.file("cas/b/obj"), 400);
    let part = rig
        .cache
        .get("cas/b/obj", Some(ByteRange::new(0, 100)))
        .await
        .unwrap();
    assert_eq!(part, &body[..100]);
    assert!(rig.calls(Op::Get) > gets);
    assert_eq!(std::fs::read(rig.file("cas/b/obj")).unwrap(), body);

    let gets = rig.calls(Op::Get);
    open_up(&rig.file("cas/b/obj"));
    std::fs::OpenOptions::new()
        .append(true)
        .open(rig.file("cas/b/obj"))
        .unwrap()
        .write_all(&[7; 50])
        .unwrap();
    assert_eq!(rig.get("cas/b/obj").await.unwrap(), body);
    assert!(rig.calls(Op::Get) > gets);
    assert_eq!(std::fs::read(rig.file("cas/b/obj")).unwrap(), body);

    // A file of the recorded size is a hit.
    let gets = rig.calls(Op::Get);
    assert_eq!(rig.get("cas/b/obj").await.unwrap(), body);
    assert_eq!(rig.calls(Op::Get), gets);
}

/// Asserts that a fill and the copy of a put each end their stage file through the disk's
/// `finish_stage`, which hands the bytes to the operating system and no more (a drive flush is
/// not part of a fill): the disk counts one end for each of 3 fills and 1 put.
#[tokio::test]
async fn each_fill_and_each_keep_ends_its_stage_file_once() {
    let rig = Rig::writing("tsize-finish");
    for n in 0..3u8 {
        let key = format!("cas/b/{n}");
        rig.put(&key, &pattern(500)).await;
        rig.get(&key).await.unwrap();
    }
    let body = Bytes::from(pattern(300));
    let made = rig.cache.put_stream(
        "cas/b/kept",
        Some(300),
        stream::once(async { Ok(body) }).boxed(),
    );
    assert_eq!(made.await, Ok(()));
    assert_eq!(rig.disk.finished(), 4);
}
