//! The fill of a blob inside a pack the cache holds whole: read from the copy, with nothing asked
//! of the upstream, and the cases where the copy is not read or does not give the blob.

use std::os::unix::fs::PermissionsExt as _;

use bytes::Bytes;
use futures::StreamExt as _;
use futures::stream;

use crate::server::local_cache::layout::{Origin, Sidecar, meta_path, name_of};
use crate::server::local_cache::metrics::{FailedFill, read};
use crate::server::local_cache::pack_testing::Pack;
use crate::server::local_cache::prefetch::Prefetched;
use crate::server::local_cache::source::blob_path;
use crate::server::local_cache::testing::{Rig, until};
use crate::server::treefs::testing::{Logged, logged};
use crate::{Backend as _, Op, PutOutcome, TreeDigest};

/// The calls the upstream has had that read an object: GETs and HEADs.
fn reads(rig: &Rig) -> (u64, u64) {
    (rig.calls(Op::Get), rig.calls(Op::Head))
}

/// The pack at `key` of blobs of `sizes`, put through the cache as the fill socket's `put` puts
/// a job's pack (a create-only put that names its SHA-256), and kept by the cache whole.
async fn kept(rig: &Rig, key: &str, sizes: &[usize]) -> Pack {
    let pack = Pack::new(key, sizes);
    let bytes = Bytes::copy_from_slice(pack.bytes());
    let (len, sha256) = (bytes.len() as u64, TreeDigest::of(&bytes));
    let body = stream::once(async move { Ok(bytes) }).boxed();
    let put = rig.cache.put_stream_sha256(key, len, body, sha256).await;
    assert_eq!(put.unwrap(), PutOutcome::Created);
    assert!(rig.has_file(key), "the cache keeps the pack whole");
    pack
}

/// The bytes the cache keeps for the blob `i` of `pack`.
fn blob_bytes(rig: &Rig, pack: &Pack, i: usize) -> Vec<u8> {
    std::fs::read(blob_path(&rig.config.path, &pack.blobs[i].0)).unwrap()
}

/// Writes `bytes` over the cache's copy of `key` (its file is mode 0444).
fn overwrite_copy(rig: &Rig, key: &str, bytes: &[u8]) {
    let file = rig.file(key);
    std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o644)).unwrap();
    std::fs::write(&file, bytes).unwrap();
}

/// Asserts that the blobs of a pack the node put and kept whole are filled from that copy, with
/// no GET and no HEAD at the upstream: each is kept as the entry named by its digest, with
/// exactly its bytes and the origin `span`, each fill is counted, nothing is left in the stage
/// directory, and the cache holds the pack and the three blobs. This is the first read of a tree
/// on the node that wrote it. Catches a fill that reads the span from the upstream again.
#[tokio::test]
async fn the_blobs_of_a_pack_the_node_kept_are_filled_with_no_upstream_read() {
    let rig = Rig::writing("theld-tree");
    let pack = kept(&rig, "cas/b/1/n/pack", &[3000, 5000, 700]).await;
    let before = reads(&rig);
    for i in 0..3 {
        rig.cache.fill_blob(&pack.blob(i)).await.unwrap();
        assert_eq!(blob_bytes(&rig, &pack, i), pack.content(i));
        let key = format!("cas/l/{}", pack.blobs[i].0);
        let text = std::fs::read(meta_path(&rig.config.path, &name_of(&key).hex)).unwrap();
        let sidecar: Sidecar = serde_json::from_slice(&text).unwrap();
        assert_eq!(sidecar.origin, Origin::Span);
    }
    assert_eq!(reads(&rig), before, "nothing was read from the upstream");
    assert_eq!(read(&rig.cache.counters().fills), 3);
    assert_eq!(rig.staged_files(), 0);
    assert_eq!(rig.cache.held(), (2 * 8700, 4));
}

/// Asserts that a blob larger than `fill_chunk` is read from the copy one chunk at a time from
/// its offset, and the chunks join into the blob, with no read at the upstream. Catches a read of
/// the copy from the wrong place after the first chunk.
#[tokio::test]
async fn a_blob_larger_than_a_chunk_is_read_from_the_copy_in_chunks() {
    let rig = Rig::with("theld-chunks", |config| {
        config.write_through = true;
        config.fill_chunk = 4096;
    });
    let pack = kept(&rig, "cas/b/2/n/pack", &[100, 10_000, 50]).await;
    let before = reads(&rig);
    rig.cache.fill_blob(&pack.blob(1)).await.unwrap();
    assert_eq!(blob_bytes(&rig, &pack, 1), pack.content(1));
    assert_eq!(reads(&rig), before);
}

/// Asserts that a blob the prefetch queue fills (the blobs of a tree a hydrate names) is read from
/// the copy of its pack too: the worker keeps it, and the upstream is read for nothing.
#[tokio::test]
async fn a_prefetched_blob_of_a_kept_pack_is_read_from_the_copy() {
    let rig = Rig::with("theld-prefetch", |config| {
        config.write_through = true;
        config.prefetch.concurrency = 1;
    });
    let pack = kept(&rig, "cas/b/3/n/pack", &[400, 600]).await;
    let before = reads(&rig);
    assert_eq!(rig.cache.prefetch(&pack.blob(1)), Prefetched::Queued);
    let file = blob_path(&rig.config.path, &pack.blobs[1].0);
    until("the worker kept the blob", || file.is_file()).await;
    assert_eq!(blob_bytes(&rig, &pack, 1), pack.content(1));
    assert_eq!(reads(&rig), before);
}

/// Asserts that a copy whose span does not hash to the blob (its bytes changed on the disk) does
/// not give the blob: the fill is made again from the upstream, which gives the right bytes, with
/// one GET of the pack, and a line at warn says so; the copy's bytes are kept nowhere, and no fill
/// is counted as failed. Catches a fill that keeps or serves the copy's bytes unchecked, or that
/// fails the read when the upstream has the blob.
#[tokio::test]
async fn a_copy_whose_span_is_not_the_blob_is_passed_over_for_the_upstream() {
    let rig = Rig::writing("theld-mismatch");
    let key = "cas/b/4/n/pack";
    let pack = kept(&rig, key, &[300, 900]).await;
    let mut wrong = pack.bytes().to_vec();
    wrong[300..].iter_mut().for_each(|byte| *byte ^= 0xff);
    overwrite_copy(&rig, key, &wrong);
    let log = Logged::default();
    let guard = logged(&log, tracing::Level::WARN);
    rig.cache.fill_blob(&pack.blob(1)).await.unwrap();
    drop(guard);
    assert_eq!(blob_bytes(&rig, &pack, 1), pack.content(1));
    assert_eq!(rig.calls_on(Op::Get, key), 1);
    let failed = rig.cache.counters().failures(FailedFill::HashMismatch);
    assert_eq!(failed, 0);
    let sha = &pack.blobs[1].0;
    let line = format!(
        "tierstore: the copy of {key} in the cache does not give the blob {sha} at 300: it is \
         read from the upstream"
    );
    assert_eq!(log.text().lines().collect::<Vec<_>>(), [line.as_str()]);
    assert_eq!(rig.staged_files(), 0);
}

/// Asserts that a copy that is not the size the cache recorded (a power cut left it short) is not
/// read, and one whose file is gone is not either: each blob is filled from the upstream, one GET
/// each, as it was before the copy was looked for, and nothing is said.
#[tokio::test]
async fn a_copy_that_is_short_or_gone_is_not_read() {
    let rig = Rig::writing("theld-short");
    let (short, gone) = ("cas/b/5/n/pack", "cas/b/6/n/pack");
    let first = kept(&rig, short, &[200, 200]).await;
    let second = kept(&rig, gone, &[300]).await;
    overwrite_copy(&rig, short, &first.bytes()[..399]);
    std::fs::remove_file(rig.file(gone)).unwrap();
    let log = Logged::default();
    let guard = logged(&log, tracing::Level::WARN);
    rig.cache.fill_blob(&first.blob(0)).await.unwrap();
    rig.cache.fill_blob(&second.blob(0)).await.unwrap();
    drop(guard);
    assert_eq!(blob_bytes(&rig, &first, 0), first.content(0));
    assert_eq!(blob_bytes(&rig, &second, 0), second.content(0));
    let gets = (rig.calls_on(Op::Get, short), rig.calls_on(Op::Get, gone));
    assert_eq!(gets, (1, 1));
    assert_eq!(log.text(), "");
}

/// Asserts that a blob whose span ends past the end of the copy is read from the upstream: the
/// cache kept an older, shorter object of the key, and the upstream now holds a longer pack. A
/// blob inside the copy is read from it, checked by its digest (here the older object's first
/// bytes are the blob's), and the one past its end costs one GET.
#[tokio::test]
async fn a_span_past_the_end_of_the_copy_is_read_from_the_upstream() {
    let rig = Rig::writing("theld-past");
    let key = "cas/b/7/n/pack";
    let _older = kept(&rig, key, &[100]).await;
    let pack = Pack::new(key, &[60, 500]);
    rig.replace(key, pack.bytes()).await;
    let before = rig.calls_on(Op::Get, key);
    rig.cache.fill_blob(&pack.blob(0)).await.unwrap();
    assert_eq!(rig.calls_on(Op::Get, key), before, "inside the copy");
    rig.cache.fill_blob(&pack.blob(1)).await.unwrap();
    assert_eq!(rig.calls_on(Op::Get, key), before + 1, "past its end");
    assert_eq!(blob_bytes(&rig, &pack, 0), pack.content(0));
    assert_eq!(blob_bytes(&rig, &pack, 1), pack.content(1));
}
