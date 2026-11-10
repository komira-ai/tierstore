//! The fills of the fill socket: a blob inside a larger object of the upstream, a whole object
//! named by its digest, and the recency `touch` sets. What each keeps and refuses, the pool of
//! permits they wait on, and that they share a fetch.

use std::os::unix::fs::PermissionsExt as _;
use std::sync::atomic::Ordering;
use std::sync::{Arc, Condvar, Mutex};

use super::fill::Stop;
use super::hold::Trace;
use super::layout::{Sidecar, meta_path, name_of};
use super::metrics::{FailedFill, read};
use super::pack_testing::Pack;
use super::source::{Blob, FillError, blob_path, status_of};
use super::testing::{Quirk, Rig, digest, pattern, until};
use super::{Mode, Tick, locked};
use crate::{Backend, Error, Op};

/// The number of entries in the stage directory of the rig.
fn staged(rig: &Rig) -> usize {
    rig.staged_files()
}

/// Asserts that a blob inside a larger object is kept under the reserved key `_tiering/fill/<sha256>`:
/// the file holds exactly the blob's bytes, mode 0444, at the path the layout gives that key; the
/// sidecar names the key, the size and the digest; the upstream was asked for the range and not
/// for the object (no HEAD, one GET); nothing is left in the stage directory; and the fill is
/// counted. A defect that kept the whole pack, or the wrong range, would show in the bytes.
#[tokio::test]
async fn a_blob_inside_a_larger_object_is_kept_under_its_reserved_key() {
    let rig = Rig::new("tsrc-blob");
    let pack = Pack::put(&rig, "packs/one", &[3000, 5000, 700]).await;
    rig.cache.fill_blob(&pack.blob(1)).await.unwrap();
    let sha = &pack.blobs[1].0;
    let file = blob_path(&rig.config.path, sha);
    assert_eq!(std::fs::read(&file).unwrap(), pack.content(1));
    assert_eq!(
        std::fs::metadata(&file).unwrap().permissions().mode() & 0o777,
        0o444
    );
    let key = format!("_tiering/fill/{sha}");
    assert_eq!(file, rig.file(&key));
    let text = std::fs::read(meta_path(&rig.config.path, &name_of(&key).hex)).unwrap();
    let sidecar: Sidecar = serde_json::from_slice(&text).unwrap();
    assert_eq!((sidecar.key.as_str(), sidecar.size), (key.as_str(), 5000));
    assert_eq!(&sidecar.sha256, sha);
    assert_eq!((rig.calls(Op::Head), rig.calls(Op::Get)), (0, 1));
    assert_eq!(staged(&rig), 0);
    assert_eq!(rig.cache.held(), (5000, 1));
    assert_eq!(read(&rig.cache.counters().fills), 1);
}

/// Asserts that a blob larger than `fill_chunk` is read in ranges of that size from the offset of
/// the blob on (3 reads for 10,000 bytes at 4096), so a fill holds one chunk in memory whatever
/// the blob's size, and the pieces join into the blob.
#[tokio::test]
async fn a_blob_is_read_in_chunks_from_its_offset() {
    let rig = Rig::with("tsrc-chunks", |config| config.fill_chunk = 4096);
    let pack = Pack::put(&rig, "packs/two", &[100, 10_000]).await;
    rig.cache.fill_blob(&pack.blob(1)).await.unwrap();
    assert_eq!(rig.calls(Op::Get), 3);
    let file = blob_path(&rig.config.path, &pack.blobs[1].0);
    assert_eq!(std::fs::read(file).unwrap(), pack.content(1));
}

/// Asserts that a fill of a blob that is in the cache asks nothing of the upstream and counts
/// the blob as used: the blob filled first is the least recently used until it is asked for
/// again, and then it is the most recently used.
#[tokio::test]
async fn a_fill_of_a_blob_already_kept_asks_nothing_and_counts_as_a_use() {
    let rig = Rig::new("tsrc-again");
    let pack = Pack::put(&rig, "packs/three", &[400, 500]).await;
    rig.cache.fill_blob(&pack.blob(0)).await.unwrap();
    rig.cache.fill_blob(&pack.blob(1)).await.unwrap();
    let first = name_of(&format!("_tiering/fill/{}", pack.blobs[0].0)).hex;
    let order = |rig: &Rig| locked(&rig.cache.inner.index).oldest_first();
    assert_eq!(order(&rig)[0].0, first);
    rig.cache.fill_blob(&pack.blob(0)).await.unwrap();
    assert_eq!(order(&rig)[1].0, first);
    assert_eq!(rig.calls(Op::Get), 2);
    assert_eq!(read(&rig.cache.counters().fills), 2);
}

/// Asserts that bytes that do not hash to the digest they were asked under are refused
/// (`mismatch`), counted as a hash mismatch, and leave nothing: no file, no sidecar, no stage
/// file, no entry. The wrong offset is the way a client's pack table could be off.
#[tokio::test]
async fn bytes_that_do_not_hash_to_the_digest_are_a_mismatch_and_leave_nothing() {
    let rig = Rig::new("tsrc-mismatch");
    let pack = Pack::put(&rig, "packs/four", &[600, 800]).await;
    let wrong = Blob {
        offset: pack.blobs[1].1 - 1,
        ..pack.blob(1)
    };
    assert_eq!(rig.cache.fill_blob(&wrong).await, Err(FillError::Mismatch));
    assert!(!blob_path(&rig.config.path, &pack.blobs[1].0).exists());
    assert_eq!((staged(&rig), rig.cache.held()), (0, (0, 0)));
    let counters = rig.cache.counters();
    assert_eq!(counters.failures(FailedFill::HashMismatch), 1);
    assert_eq!(read(&counters.fills), 0);
}

/// Asserts the failures of the upstream: a pack that is not there is `upstream 404`, an upstream
/// that does not answer is `upstream 503` and is counted as an upstream error, a range that starts
/// past the end of the object is `upstream 500`, a range that ends past it is `short`, and a body
/// shorter than asked is `short` too; none leaves a file or a stage file.
#[tokio::test]
async fn the_failures_of_the_upstream_are_answered_with_their_status() {
    let rig = Rig::new("tsrc-upstream");
    let pack = Pack::put(&rig, "packs/five", &[500]).await;
    let sha = pack.blobs[0].0.as_str();
    let blob = |from, offset, size| Blob {
        sha256: sha,
        size,
        from,
        offset,
    };

    let missing = blob("packs/none", 0, 500);
    assert_eq!(
        rig.cache.fill_blob(&missing).await,
        Err(FillError::Upstream(404))
    );
    assert_eq!(rig.cache.counters().failures(FailedFill::UpstreamError), 0);

    rig.upstream
        .fail_next_with(Op::Get, Error::Unavailable("down".to_owned()));
    assert_eq!(
        rig.cache.fill_blob(&pack.blob(0)).await,
        Err(FillError::Upstream(503))
    );
    assert_eq!(rig.cache.counters().failures(FailedFill::UpstreamError), 1);

    let past = blob("packs/five", 501, 10);
    assert_eq!(
        rig.cache.fill_blob(&past).await,
        Err(FillError::Upstream(500))
    );
    let over = blob("packs/five", 400, 200);
    assert_eq!(rig.cache.fill_blob(&over).await, Err(FillError::Short));
    rig.tamper.short.store(true, Ordering::SeqCst);
    assert_eq!(
        rig.cache.fill_blob(&pack.blob(0)).await,
        Err(FillError::Short)
    );
    assert_eq!(rig.cache.counters().failures(FailedFill::ShortBody), 2);
    assert_eq!((staged(&rig), rig.cache.held()), (0, (0, 0)));
}

/// Asserts the status each failure of the upstream stands for.
#[test]
fn each_failure_of_the_upstream_has_its_status() {
    let named = |f: fn(String) -> Error| f("k".to_owned());
    let table = [
        (named(Error::NotFound), 404),
        (named(Error::Reserved), 403),
        (named(Error::InvalidKey), 400),
        (named(Error::InvalidSegment), 400),
        (named(Error::Unsupported), 501),
        (named(Error::Unavailable), 503),
        (named(Error::SlowDown), 503),
        (named(Error::Other), 500),
        (named(Error::Full), 500),
    ];
    for (error, status) in table {
        assert_eq!(status_of(&error), status, "{error}");
    }
}

/// Asserts that a blob larger than the budget is `no space` without a call to the upstream, and
/// that one that fits the budget but would take the cache and its running fills past the critical
/// mark is `no space` too, counted as a skipped fill, and leaves nothing.
#[tokio::test]
async fn a_blob_that_does_not_fit_is_no_space() {
    let rig = Rig::new("tsrc-space");
    let pack = Pack::put(&rig, "packs/six", &[1_000_000, 2_000_000]).await;
    assert_eq!(
        rig.cache.fill_blob(&pack.blob(1)).await,
        Err(FillError::NoSpace)
    );
    assert_eq!(rig.calls(Op::Get), 0);
    assert_eq!(
        rig.cache.fill_blob(&pack.blob(0)).await,
        Err(FillError::NoSpace)
    );
    assert_eq!(read(&rig.cache.counters().fills_skipped), 1);
    assert_eq!((staged(&rig), rig.cache.held()), (0, (0, 0)));
}

/// Asserts that a cache that does not hold the lock of its directory fills nothing: the answer is
/// `read-only`, the upstream is not asked and the directory is not written; and a `touch` in it
/// changes nothing.
#[tokio::test]
async fn a_read_only_cache_fills_nothing() {
    let rig = Rig::new("tsrc-readonly");
    let pack = Pack::put(&rig, "packs/seven", &[300]).await;
    let holder = rig.locked();
    let waiting = rig.locked();
    assert_eq!(waiting.mode(), Mode::ReadOnly);
    assert_eq!(
        waiting.fill_blob(&pack.blob(0)).await,
        Err(FillError::ReadOnly)
    );
    assert_eq!(
        waiting.fill_manifest(&digest(b"x")).await,
        Err(FillError::ReadOnly)
    );
    waiting.touch_blobs([pack.blobs[0].0.as_str()]);
    assert_eq!(rig.calls(Op::Get) + rig.calls(Op::Head), 0);
    assert!(!blob_path(&rig.config.path, &pack.blobs[0].0).exists());
    drop(holder);
}

/// Asserts that a fill that is running when the cache loses the lock of its directory writes
/// nothing more and is answered `read-only`: the lock file is replaced and a tick notices while
/// the upstream's answer is held.
#[tokio::test]
async fn a_fill_that_loses_the_lock_in_flight_is_read_only_and_leaves_nothing() {
    let rig = Rig::new("tsrc-lost");
    let pack = Pack::put(&rig, "packs/eight", &[300]).await;
    let cache = rig.locked();
    let gate = rig.tamper.hold();
    let filling = {
        let (cache, sha) = (cache.clone(), pack.blobs[0].0.clone());
        let (key, offset) = (pack.key.clone(), pack.blobs[0].1);
        tokio::spawn(async move {
            let blob = Blob {
                sha256: &sha,
                size: 300,
                from: &key,
                offset,
            };
            cache.fill_blob(&blob).await
        })
    };
    until("the read reached the upstream", || rig.calls(Op::Get) == 1).await;
    std::fs::remove_file(rig.config.path.join(".lock")).unwrap();
    assert_eq!(cache.tick().unwrap(), Tick::Lost);
    gate.add_permits(4);
    assert_eq!(filling.await.unwrap(), Err(FillError::ReadOnly));
    assert!(!blob_path(&rig.config.path, &pack.blobs[0].0).exists());
    assert_eq!(staged(&rig), 0);
}

/// Asserts that concurrent fills of one blob share one fetch: eight requests, one GET, eight
/// answers `ok`, one fill counted, and the table of the socket's fills is empty again.
#[tokio::test]
async fn concurrent_fills_of_one_blob_share_one_fetch() {
    let rig = Rig::new("tsrc-share");
    let pack = Pack::put(&rig, "packs/nine", &[900]).await;
    let gate = rig.tamper.hold();
    let mut requests = Vec::new();
    for _ in 0..8 {
        let (cache, sha) = (rig.cache.clone(), pack.blobs[0].0.clone());
        let key = pack.key.clone();
        requests.push(tokio::spawn(async move {
            let blob = Blob {
                sha256: &sha,
                size: 900,
                from: &key,
                offset: 0,
            };
            cache.fill_blob(&blob).await
        }));
        tokio::task::yield_now().await;
    }
    until("the one read reached the upstream", || {
        rig.calls(Op::Get) == 1
    })
    .await;
    gate.add_permits(16);
    for request in requests {
        assert_eq!(request.await.unwrap(), Ok(()));
    }
    assert_eq!(rig.calls(Op::Get), 1);
    assert_eq!(read(&rig.cache.counters().fills), 1);
    assert!(locked(&rig.cache.inner.source_flights).is_empty());
    assert_eq!(rig.cache.inner.source_permits.available_permits(), 16);
}

/// Asserts the first scenario of the design's PR list: twenty cold fills, with every permit of the
/// S3 path's pool taken, all complete; sixteen run at once, the other four wait for a permit of the
/// socket's pool and get one; none is skipped or passed through (no skipped fill is counted), and
/// the S3 path's permits are still all held by their taker, so no fill used one.
#[tokio::test]
async fn twenty_cold_fills_with_the_s3_paths_permits_all_taken_complete_and_none_passes_through() {
    let rig = Rig::with("tsrc-twenty", |config| {
        config.max_concurrent_fills = 2;
        config.budget = 16 << 20;
    });
    let pack = Pack::put(&rig, "packs/twenty", &[1000; 20]).await;
    let s3_permits = Arc::clone(&rig.cache.inner.permits);
    let held = Arc::clone(&s3_permits).try_acquire_many_owned(2).unwrap();
    let gate = rig.tamper.hold();
    let mut requests = Vec::new();
    for i in 0..20 {
        let (cache, sha) = (rig.cache.clone(), pack.blobs[i].0.clone());
        let (key, offset) = (pack.key.clone(), pack.blobs[i].1);
        requests.push(tokio::spawn(async move {
            let blob = Blob {
                sha256: &sha,
                size: 1000,
                from: &key,
                offset,
            };
            cache.fill_blob(&blob).await
        }));
    }
    until("sixteen reads are at the upstream", || {
        rig.calls(Op::Get) == 16
    })
    .await;
    assert_eq!(rig.cache.inner.source_permits.available_permits(), 0);
    gate.add_permits(64);
    for request in requests {
        assert_eq!(request.await.unwrap(), Ok(()));
    }
    let counters = rig.cache.counters();
    assert_eq!(read(&counters.fills), 20);
    assert_eq!(read(&counters.fills_skipped), 0);
    assert_eq!(read(&counters.fills_skipped_busy), 0);
    assert_eq!(s3_permits.available_permits(), 0);
    drop(held);
    for i in 0..20 {
        let file = blob_path(&rig.config.path, &pack.blobs[i].0);
        assert_eq!(std::fs::read(file).unwrap(), pack.content(i));
    }
}

/// Asserts that the two pools are apart the other way too: with every permit of the socket's pool
/// taken, a miss of the S3 path fills (it takes its own permit), and the socket's pool is still
/// empty after.
#[tokio::test]
async fn the_s3_path_fills_while_every_permit_of_the_socket_is_taken() {
    let rig = Rig::new("tsrc-apart");
    let body = pattern(2000);
    let key = format!("cas/l/{}", digest(&body));
    rig.put(&key, &body).await;
    let all = rig
        .cache
        .inner
        .source_permits
        .clone()
        .try_acquire_many_owned(16)
        .unwrap();
    assert_eq!(rig.get(&key).await.unwrap(), body);
    assert_eq!(read(&rig.cache.counters().fills), 1);
    assert_eq!(rig.cache.inner.source_permits.available_permits(), 0);
    drop(all);
}

/// Asserts that a request waits for a permit of the socket's pool and is answered `busy` when none
/// comes free within `fill_socket_wait`, with the upstream untouched by it; that the fill that
/// held the permit is not hurt by the other's giving up; and that the request is served once a
/// permit is free. The clock is paused and moves by itself when every task waits.
#[tokio::test(start_paused = true)]
async fn a_request_waits_for_a_permit_and_is_busy_when_none_comes_free_in_time() {
    let rig = Rig::with("tsrc-busy", |config| {
        config.fill_socket_concurrency = 1;
        config.fill_socket_wait = std::time::Duration::from_secs(5);
    });
    let pack = Pack::put(&rig, "packs/ten", &[400, 500]).await;
    let gate = rig.tamper.hold();
    let first = {
        let (cache, sha) = (rig.cache.clone(), pack.blobs[0].0.clone());
        let key = pack.key.clone();
        tokio::spawn(async move {
            let blob = Blob {
                sha256: &sha,
                size: 400,
                from: &key,
                offset: 0,
            };
            cache.fill_blob(&blob).await
        })
    };
    until("the first fill is at the upstream", || {
        rig.calls(Op::Get) == 1
    })
    .await;
    assert_eq!(
        rig.cache.fill_blob(&pack.blob(1)).await,
        Err(FillError::Busy)
    );
    assert_eq!(rig.calls(Op::Get), 1);
    gate.add_permits(8);
    assert_eq!(first.await.unwrap(), Ok(()));
    assert_eq!(rig.cache.fill_blob(&pack.blob(1)).await, Ok(()));
}

/// Asserts that a fill whose task panics is `aborted`, counted once as such, and does not wedge
/// its blob: the table, the permit and the stage directory are empty again and the next request
/// for it is filled.
#[tokio::test]
async fn a_fill_that_panics_is_aborted_and_frees_its_blob() {
    let rig = Rig::new("tsrc-panic");
    let pack = Pack::put(&rig, "packs/eleven", &[250]).await;
    rig.tamper.panic_once.store(true, Ordering::SeqCst);
    assert_eq!(
        rig.cache.fill_blob(&pack.blob(0)).await,
        Err(FillError::Aborted)
    );
    assert!(locked(&rig.cache.inner.source_flights).is_empty());
    assert_eq!(rig.cache.inner.source_permits.available_permits(), 16);
    assert_eq!(staged(&rig), 0);
    assert_eq!(rig.cache.fill_blob(&pack.blob(0)).await, Ok(()));
}

/// Asserts that a disk that refuses the link of the finished file is `io`, counted as a local
/// failure, and leaves no stage file.
#[tokio::test]
async fn a_disk_that_cannot_link_is_io() {
    let rig = Rig::new("tsrc-io");
    let pack = Pack::put(&rig, "packs/twelve", &[250]).await;
    rig.disk
        .set(Quirk::LinkFails(std::io::ErrorKind::PermissionDenied));
    assert_eq!(rig.cache.fill_blob(&pack.blob(0)).await, Err(FillError::Io));
    assert_eq!(rig.cache.counters().failures(FailedFill::LocalIo), 1);
    assert_eq!(staged(&rig), 0);
}

/// Asserts the answer and the counter of each way a fetch can stop.
#[tokio::test]
async fn each_stop_of_a_fetch_has_its_answer() {
    let rig = Rig::new("tsrc-stops");
    let inner = &rig.cache.inner;
    let counters = rig.cache.counters();
    let blocked = Stop::Upstream(Error::Unavailable("x".to_owned()));
    assert_eq!(inner.stopped(blocked), FillError::Upstream(503));
    assert_eq!(inner.stopped(Stop::Short), FillError::Short);
    assert_eq!(inner.stopped(Stop::Local), FillError::Io);
    assert_eq!(inner.stopped(Stop::Changed), FillError::Changed);
    assert_eq!(inner.stopped(Stop::Invalidated), FillError::Changed);
    assert_eq!(inner.stopped(Stop::Lost), FillError::ReadOnly);
    assert_eq!(inner.stopped(Stop::Mismatch), FillError::Mismatch);
    assert_eq!(counters.failures(FailedFill::UpstreamError), 1);
    assert_eq!(counters.failures(FailedFill::ShortBody), 1);
    assert_eq!(counters.failures(FailedFill::LocalIo), 1);
    assert_eq!(counters.failures(FailedFill::Changed), 2);
    assert_eq!(counters.failures(FailedFill::HashMismatch), 1);
    assert_eq!(read(&counters.fills_skipped), 1);
}

/// Asserts that the object `cas/l/<sha256>` is filled whole by its digest alone: the file is the
/// object's bytes at the path the layout gives the key, one HEAD and one GET reached the
/// upstream, and a read of the key through the S3 path afterwards is a hit.
#[tokio::test]
async fn a_manifest_is_filled_whole_by_its_digest() {
    let rig = Rig::new("tsrc-manifest");
    let body = pattern(3000);
    let sha = digest(&body);
    let key = format!("cas/l/{sha}");
    rig.put(&key, &body).await;
    rig.cache.fill_manifest(&sha).await.unwrap();
    assert_eq!(std::fs::read(rig.file(&key)).unwrap(), body);
    assert_eq!((rig.calls(Op::Head), rig.calls(Op::Get)), (1, 1));
    assert_eq!(rig.get(&key).await.unwrap(), body);
    assert_eq!(read(&rig.cache.counters().hits), 1);
    assert_eq!(rig.calls(Op::Get), 1);
}

/// Asserts the refusals of a whole-object fill: an object that is not there is `upstream 404`; one
/// the upstream gives no content tag for is `upstream 501`; one whose bytes are not the digest it
/// is named by is `mismatch` and is not kept; one over 64 MiB is `too large` before a byte is read.
#[tokio::test]
async fn a_manifest_that_cannot_be_filled_is_refused_by_its_reason() {
    let rig = Rig::new("tsrc-manifest-refused");
    assert_eq!(
        rig.cache.fill_manifest(&digest(b"none")).await,
        Err(FillError::Upstream(404))
    );

    let body = pattern(500);
    let sha = digest(&body);
    rig.put(&format!("cas/l/{sha}"), &body).await;
    rig.tamper.blank_tags.store(true, Ordering::SeqCst);
    assert_eq!(
        rig.cache.fill_manifest(&sha).await,
        Err(FillError::Upstream(501))
    );
    rig.tamper.blank_tags.store(false, Ordering::SeqCst);

    let named = digest(b"what it is called");
    rig.put(&format!("cas/l/{named}"), &body).await;
    assert_eq!(
        rig.cache.fill_manifest(&named).await,
        Err(FillError::Mismatch)
    );
    assert!(!rig.has_file(&format!("cas/l/{named}")));

    let huge = vec![0_u8; (64 << 20) + 1];
    let huge_sha = digest(&huge);
    rig.put(&format!("cas/l/{huge_sha}"), &huge).await;
    let gets = rig.calls(Op::Get);
    assert_eq!(
        rig.cache.fill_manifest(&huge_sha).await,
        Err(FillError::TooLarge)
    );
    assert_eq!(rig.calls(Op::Get), gets);
    assert_eq!(rig.cache.held(), (0, 0));
}

/// Asserts that a whole-object request that waited for a permit while the S3 path filled the same
/// object finds it in the cache and fetches nothing: one GET in all, the S3 path's.
#[tokio::test]
async fn a_request_that_waited_finds_the_object_a_reader_filled_meanwhile() {
    let rig = Rig::with("tsrc-waited", |config| config.fill_socket_concurrency = 1);
    let pack = Pack::put(&rig, "packs/thirteen", &[300]).await;
    let body = pattern(1200);
    let sha = digest(&body);
    let key = format!("cas/l/{sha}");
    rig.put(&key, &body).await;
    let gate = rig.tamper.hold_key(&pack.key);
    let holder = {
        let (cache, sha) = (rig.cache.clone(), pack.blobs[0].0.clone());
        let key = pack.key.clone();
        tokio::spawn(async move {
            let blob = Blob {
                sha256: &sha,
                size: 300,
                from: &key,
                offset: 0,
            };
            cache.fill_blob(&blob).await
        })
    };
    until("the holder is at the upstream", || rig.calls(Op::Get) == 1).await;
    let waiting = {
        let (cache, sha) = (rig.cache.clone(), sha.clone());
        tokio::spawn(async move { cache.fill_manifest(&sha).await })
    };
    tokio::task::yield_now().await;
    assert_eq!(rig.get(&key).await.unwrap(), body);
    gate.add_permits(8);
    assert_eq!(holder.await.unwrap(), Ok(()));
    assert_eq!(waiting.await.unwrap(), Ok(()));
    assert_eq!(rig.calls_on(Op::Get, &key), 1);
}

/// Asserts that two requests that wait for the permits of the pool together, when two come free
/// at once, run one fetch: both answered `ok`, one GET of the object.
#[tokio::test]
async fn two_requests_that_get_permits_together_share_the_fetch() {
    let rig = Rig::with("tsrc-together", |config| config.fill_socket_concurrency = 2);
    let pack = Pack::put(&rig, "packs/fourteen", &[300, 300]).await;
    let body = pattern(1500);
    let sha = digest(&body);
    let key = format!("cas/l/{sha}");
    rig.put(&key, &body).await;
    let gate = rig.tamper.hold_key(&pack.key);
    let mut holders = Vec::new();
    for i in 0..2 {
        let (cache, sha) = (rig.cache.clone(), pack.blobs[i].0.clone());
        let (key, offset) = (pack.key.clone(), pack.blobs[i].1);
        holders.push(tokio::spawn(async move {
            let blob = Blob {
                sha256: &sha,
                size: 300,
                from: &key,
                offset,
            };
            cache.fill_blob(&blob).await
        }));
    }
    until("both holders are at the upstream", || {
        rig.calls(Op::Get) == 2
    })
    .await;
    let mut waiting = Vec::new();
    for _ in 0..2 {
        let (cache, sha) = (rig.cache.clone(), sha.clone());
        waiting.push(tokio::spawn(async move { cache.fill_manifest(&sha).await }));
    }
    tokio::task::yield_now().await;
    gate.add_permits(8);
    for request in holders.into_iter().chain(waiting) {
        assert_eq!(request.await.unwrap(), Ok(()));
    }
    assert_eq!(rig.calls_on(Op::Get, &key), 1);
}

/// Asserts that `touch` makes the blobs it names the most recently used, whether they are blobs
/// of the fill socket or whole objects, in the order named, and passes over a digest the cache
/// does not hold.
#[tokio::test]
async fn touch_counts_blobs_and_objects_as_used() {
    let rig = Rig::new("tsrc-touch");
    let pack = Pack::put(&rig, "packs/fifteen", &[100, 200]).await;
    let body = pattern(300);
    let sha = digest(&body);
    rig.put(&format!("cas/l/{sha}"), &body).await;
    rig.cache.fill_blob(&pack.blob(0)).await.unwrap();
    rig.cache.fill_manifest(&sha).await.unwrap();
    rig.cache.fill_blob(&pack.blob(1)).await.unwrap();
    let blob = |i: usize| name_of(&format!("_tiering/fill/{}", pack.blobs[i].0)).hex;
    let order = || -> Vec<String> {
        let all = locked(&rig.cache.inner.index).oldest_first();
        all.into_iter().map(|(hex, _)| hex).collect()
    };
    assert_eq!(order(), [blob(0), sha.clone(), blob(1)]);
    let unknown = digest(b"nobody has this");
    rig.cache
        .touch_blobs([pack.blobs[0].0.as_str(), unknown.as_str(), sha.as_str()]);
    assert_eq!(order(), [blob(1), blob(0), sha]);
}

/// Asserts that an entry whose file is gone (an eviction took it while a fill linked it) is not
/// believed: the next request drops the entry and fetches the blob again, and the file is there.
#[tokio::test]
async fn an_entry_whose_file_is_gone_is_filled_again() {
    let rig = Rig::new("tsrc-gone");
    let pack = Pack::put(&rig, "packs/sixteen", &[350]).await;
    rig.cache.fill_blob(&pack.blob(0)).await.unwrap();
    let file = blob_path(&rig.config.path, &pack.blobs[0].0);
    std::fs::remove_file(&file).unwrap();
    rig.cache.fill_blob(&pack.blob(0)).await.unwrap();
    assert_eq!(std::fs::read(&file).unwrap(), pack.content(0));
    assert_eq!(rig.calls(Op::Get), 2);
    assert_eq!(rig.cache.held(), (350, 1));
}

/// A point where a thread waits until the test lets it go.
#[derive(Default)]
struct Latch {
    state: Mutex<(bool, bool)>,
    changed: Condvar,
}

impl Latch {
    /// Says it is reached, and waits to be let go.
    fn wait(&self) {
        let mut state = self.state.lock().unwrap();
        state.0 = true;
        self.changed.notify_all();
        while !state.1 {
            state = self.changed.wait(state).unwrap();
        }
    }

    fn reached(&self) -> bool {
        self.state.lock().unwrap().0
    }

    fn release(&self) {
        self.state.lock().unwrap().1 = true;
        self.changed.notify_all();
    }
}

/// Asserts the outcome of a fill that races an eviction of the same blob: the pass has chosen the
/// blob and not yet removed it when the fill runs, so the fill finds it in the cache and answers
/// `ok`, and the pass then removes it. The answer is about the moment it was given: the blob is
/// gone, its entry with its file, the index and the directory agree, and the next request fetches it
/// again and keeps it.
#[tokio::test]
async fn a_fill_racing_an_eviction_leaves_the_cache_whole_and_the_next_request_fetches_again() {
    let rig = Rig::new("tsrc-race");
    let pack = Pack::put(&rig, "packs/seventeen", &[1500, 1500]).await;
    rig.cache.fill_blob(&pack.blob(0)).await.unwrap();
    rig.cache.fill_blob(&pack.blob(1)).await.unwrap();
    let latch = Arc::new(Latch::default());
    let between = {
        let latch = Arc::clone(&latch);
        Arc::new(move || latch.wait())
    };
    rig.cache.inner.set_trace(Trace {
        between_delete_steps: Some(between),
        ..Trace::default()
    });
    // The file system is full: the pass evicts the least recently used blob.
    rig.disk.set_space(1000, 0);
    let pass = {
        let inner = Arc::clone(&rig.cache.inner);
        tokio::task::spawn_blocking(move || inner.evict_pass())
    };
    until("the pass is at the blob it evicts", || latch.reached()).await;
    assert_eq!(rig.cache.fill_blob(&pack.blob(0)).await, Ok(()));
    latch.release();
    assert_eq!(pass.await.unwrap().evicted, 1);
    rig.cache.inner.set_trace(Trace::default());
    rig.disk.set_space(1 << 40, 1 << 40);
    let file = blob_path(&rig.config.path, &pack.blobs[0].0);
    assert!(!file.exists());
    assert_eq!(rig.cache.held(), (1500, 1));
    assert_eq!(rig.cache.fill_blob(&pack.blob(0)).await, Ok(()));
    assert_eq!(std::fs::read(&file).unwrap(), pack.content(0));
    assert_eq!(rig.calls(Op::Get), 3);
    assert_eq!(rig.cache.held(), (3000, 2));
}

/// Asserts that the pass a fill starts, over the high-water mark, takes the older blob and keeps
/// the blob that was just filled.
#[tokio::test]
async fn the_pass_a_fill_starts_keeps_the_blob_it_just_filled() {
    let rig = Rig::with("tsrc-keeps", |config| config.budget = 4000);
    let pack = Pack::put(&rig, "packs/eighteen", &[1900, 1900]).await;
    rig.cache.fill_blob(&pack.blob(0)).await.unwrap();
    rig.cache.fill_blob(&pack.blob(1)).await.unwrap();
    assert!(!blob_path(&rig.config.path, &pack.blobs[0].0).exists());
    assert!(blob_path(&rig.config.path, &pack.blobs[1].0).exists());
    assert_eq!(rig.cache.held(), (1900, 1));
}

/// Asserts that a fill of a reserved key's blob is invisible to the S3 side of the cache: a read, a
/// ranged read, a HEAD and a streamed read of `_tiering/fill/<sha256>` are refused as reserved
/// without a call to the upstream, and no listing of the cache, of that prefix or of any other,
/// shows it.
#[tokio::test]
async fn a_filled_blob_is_invisible_to_the_s3_side() {
    let rig = Rig::new("tsrc-reserved");
    let pack = Pack::put(&rig, "packs/nineteen", &[640]).await;
    rig.cache.fill_blob(&pack.blob(0)).await.unwrap();
    let key = format!("_tiering/fill/{}", pack.blobs[0].0);
    let calls = (rig.calls(Op::Get), rig.calls(Op::Head));
    let reserved = Err(Error::Reserved(key.clone()));
    assert_eq!(rig.cache.get(&key, None).await, reserved);
    let range = crate::ByteRange::new(0, 10);
    assert_eq!(rig.cache.get(&key, Some(range)).await, reserved);
    assert_eq!(
        rig.cache.head(&key).await.map(|meta| meta.size),
        Err(Error::Reserved(key.clone()))
    );
    assert!(rig.cache.get_stream(&key, None).await.is_err());
    assert_eq!((rig.calls(Op::Get), rig.calls(Op::Head)), calls);
    for prefix in ["", "_tiering/", "_tiering/fill/", "packs/"] {
        let listed = rig.cache.list(prefix).await.unwrap();
        assert!(
            listed
                .iter()
                .all(|entry| !entry.key.starts_with("_tiering/")),
            "{prefix}"
        );
    }
    let page = rig.cache.list_page("", None, 100).await.unwrap();
    assert!(page.iter().all(|entry| !entry.key.starts_with("_tiering/")));
}

/// Asserts that the mode channel follows the lock: a cache that holds it says read-write, one that
/// waits says read-only, a tick that takes the lock changes it to read-write, and a tick that finds
/// the lock file replaced to read-only.
#[tokio::test]
async fn the_mode_channel_follows_the_lock() {
    let rig = Rig::new("tsrc-mode");
    let holder = rig.locked();
    let waiting = rig.locked();
    let mut held = holder.watch_mode();
    let mut modes = waiting.watch_mode();
    assert_eq!(*held.borrow_and_update(), Mode::ReadWrite);
    assert_eq!(*modes.borrow_and_update(), Mode::ReadOnly);
    drop(holder);
    assert_eq!(waiting.tick().unwrap(), Tick::Took);
    modes.changed().await.unwrap();
    assert_eq!(*modes.borrow_and_update(), Mode::ReadWrite);
    std::fs::remove_file(rig.config.path.join(".lock")).unwrap();
    assert_eq!(waiting.tick().unwrap(), Tick::Lost);
    modes.changed().await.unwrap();
    assert_eq!(*modes.borrow_and_update(), Mode::ReadOnly);
    assert_eq!(*rig.cache.watch_mode().borrow(), Mode::ReadWrite);
}

/// Asserts the words each way a fill can end is answered with on the socket.
#[test]
fn each_way_a_fill_can_end_has_its_words() {
    let words = [
        (FillError::Upstream(404), "upstream 404"),
        (FillError::Mismatch, "mismatch"),
        (FillError::Short, "short"),
        (FillError::NoSpace, "no space"),
        (FillError::TooLarge, "too large"),
        (FillError::Busy, "busy"),
        (FillError::ReadOnly, "read-only"),
        (FillError::Io, "io"),
        (FillError::Changed, "changed"),
        (FillError::Aborted, "aborted"),
    ];
    for (error, text) in words {
        assert_eq!(error.to_string(), text);
    }
}
