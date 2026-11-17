//! Making a file of a placement readable: `open_file` by placement and path and `fetch_span` by
//! span, over the small tree of the fixture, from a cache that holds the range and from one that
//! must fill it.

use std::os::unix::fs::FileExt as _;

use super::super::RangeError;
use super::testing::{Bench, s3_at};
use super::{FetchError, FileSpan, PlacementId, ReadError, Resolved, SweepReport};
use crate::{Mode, Op, TreeDigest};

async fn placed(name: &str) -> (Bench, PlacementId) {
    let bench = Bench::new(name).await;
    let made = bench
        .store
        .create(bench.request(vec![s3_at("jobs/1/root/")]))
        .unwrap();
    (bench, made.placement)
}

/// The calls the upstream has had: HEADs and GETs.
fn upstream_calls(bench: &Bench) -> (u64, u64) {
    (bench.rig.calls(Op::Head), bench.rig.calls(Op::Get))
}

/// The span of the blob `blob` of the pack, its size and its digest, as `fetch_span` is asked.
fn blob_of(bench: &Bench, blob: usize) -> (FileSpan, u64, TreeDigest) {
    let (sha, offset, size) = &bench.pack.blobs[blob];
    let span = FileSpan {
        pack: bench.pack.key.clone(),
        offset: *offset,
    };
    (span, *size, TreeDigest::from_hex(sha).unwrap())
}

/// The `len` bytes of `file` from `offset`.
fn read_at(file: &std::path::Path, offset: u64, len: u64) -> Vec<u8> {
    let mut bytes = vec![0; len as usize];
    std::fs::File::open(file)
        .unwrap()
        .read_exact_at(&mut bytes, offset)
        .unwrap();
    bytes
}

/// Asserts that a file whose range the cache holds opens with its mode (the setuid bit kept), its
/// size and its digest, with a span whose file holds exactly the blob's bytes at the span's
/// offset, and that the open makes no request of the upstream.
#[tokio::test]
async fn a_file_the_cache_holds_opens_without_an_upstream_request() {
    let (bench, id) = placed("trees-open-held").await;
    bench.fill_all().await;
    let before = upstream_calls(&bench);
    let open = bench.store.open_file(&id, "bin/tool").await.unwrap();
    assert_eq!(open.mode, Mode::new(0o4755).unwrap());
    let (_, size, sha) = blob_of(&bench, 0);
    assert_eq!((open.size, open.sha256), (size, sha));
    let span = open.span.as_ref().expect("a span");
    assert_eq!(
        read_at(&span.path, span.offset, open.size),
        bench.pack.content(0)
    );
    let other = bench.store.open_file(&id, "lib/deep/data").await.unwrap();
    assert_eq!(other.mode, Mode::FILE);
    let span = other.span.as_ref().expect("a span");
    assert_eq!(
        read_at(&span.path, span.offset, other.size),
        bench.pack.content(2)
    );
    assert_eq!(upstream_calls(&bench), before);
}

/// Asserts that a file the cache does not hold is filled by the open: the cache holds the range
/// after it, the span's file has the blob's bytes, and a second open of the file asks the upstream
/// for nothing more.
#[tokio::test]
async fn a_file_the_cache_lacks_is_filled_once() {
    let (bench, id) = placed("trees-open-fill").await;
    let (span, size, _) = blob_of(&bench, 1);
    assert_eq!(bench.rig.cache.span_of(&span.pack, span.offset, size), None);
    let open = bench.store.open_file(&id, "bin/other").await.unwrap();
    let held = bench
        .rig
        .cache
        .span_of(&span.pack, span.offset, size)
        .expect("the range is held");
    assert_eq!(open.span, Some(held.clone()));
    assert_eq!(
        read_at(&held.path, held.offset, size),
        bench.pack.content(1)
    );
    assert!(upstream_calls(&bench).1 > 0);
    let after = upstream_calls(&bench);
    let again = bench.store.open_file(&id, "bin/other").await.unwrap();
    assert_eq!(again.span, Some(held));
    assert_eq!(upstream_calls(&bench), after);
}

/// Asserts that the open file keeps its placement: after the placement is released and swept it is
/// still there while the open file is kept (its locations are gone), and once the open file is
/// dropped a sweep leaves nothing of it.
#[tokio::test]
async fn an_open_file_holds_its_placement_until_it_is_dropped() {
    let (bench, id) = placed("trees-open-hold").await;
    let store = &bench.store;
    let open = store.open_file(&id, "bin/tool").await.unwrap();
    assert_eq!(store.placement(&id).unwrap().reads, 1);
    store.release(&id).unwrap();
    assert_eq!(store.sweep(), SweepReport::default());
    assert!(store.placement(&id).is_some());
    assert!(store.resolve(&id, "bin/tool").is_ok());
    drop(open);
    store.sweep();
    assert!(store.placement(&id).is_none());
    assert_eq!(store.sizes(), (0, 0));
    assert_eq!(
        store.open_file(&id, "bin/tool").await.unwrap_err(),
        FetchError::Read(ReadError::NoPlacement)
    );
}

/// Asserts that the empty file opens with no span, size 0 and the digest of no bytes, and asks the
/// upstream for nothing and fills nothing.
#[tokio::test]
async fn the_empty_file_has_no_span_and_no_fill() {
    let (bench, id) = placed("trees-open-empty").await;
    let open = bench.store.open_file(&id, "zero").await.unwrap();
    assert_eq!(open.span, None);
    assert_eq!(open.size, 0);
    assert_eq!(open.sha256, TreeDigest::of(b""));
    assert_eq!(open.mode, Mode::FILE);
    assert_eq!(upstream_calls(&bench), (0, 0));
    assert_eq!(bench.rig.cache.extents_held(), (0, 0));
}

/// Asserts that a directory (the root, a named one, an empty one) and a symbolic link are refused
/// as not a file, each with what it is, and that nothing is read from the upstream for them.
#[tokio::test]
async fn a_directory_or_a_link_is_not_a_file() {
    let (bench, id) = placed("trees-open-not-file").await;
    let open = |path: &'static str| bench.store.open_file(&id, path);
    let dir = |entries| {
        FetchError::NotAFile(Resolved::Dir {
            mode: Mode::DIR,
            entries,
        })
    };
    assert_eq!(open("bin").await.unwrap_err(), dir(2));
    assert_eq!(open("").await.unwrap_err(), dir(5));
    assert_eq!(open("empty").await.unwrap_err(), dir(0));
    assert_eq!(
        open("link").await.unwrap_err(),
        FetchError::NotAFile(Resolved::Symlink {
            target: "bin/tool".to_owned()
        })
    );
    assert_eq!(upstream_calls(&bench), (0, 0));
    assert_eq!(
        FetchError::NotAFile(Resolved::Symlink {
            target: String::new()
        })
        .to_string(),
        "not a file"
    );
}

/// Asserts that a path that is not in the tree is `NotFound`, a path below a file is
/// `NotADirectory`, and a placement that is not there (one never made, and one whose lease is over)
/// is `NoPlacement`.
#[tokio::test]
async fn a_path_that_is_not_there_and_a_placement_that_is_not_there_are_refused() {
    let (bench, id) = placed("trees-open-missing").await;
    let store = &bench.store;
    for path in ["nothing", "bin/nothing", "lib/deep/nothing"] {
        assert_eq!(
            store.open_file(&id, path).await.unwrap_err(),
            FetchError::Read(ReadError::NotFound),
            "{path:?}"
        );
    }
    for path in ["bin/tool/more", "zero/more"] {
        assert_eq!(
            store.open_file(&id, path).await.unwrap_err(),
            FetchError::Read(ReadError::NotADirectory),
            "{path:?}"
        );
    }
    let nothing = PlacementId::parse(&"ab".repeat(16)).unwrap();
    assert_eq!(
        store.open_file(&nothing, "bin/tool").await.unwrap_err(),
        FetchError::Read(ReadError::NoPlacement)
    );
    bench.rig.clock.advance(5 * 3600 * 1000);
    assert_eq!(
        store.open_file(&id, "bin/tool").await.unwrap_err(),
        FetchError::Read(ReadError::NoPlacement)
    );
    assert_eq!(upstream_calls(&bench), (0, 0));
    assert_eq!(
        FetchError::Read(ReadError::NoPlacement).to_string(),
        "no such placement"
    );
}

/// Asserts that a range asked under a digest the bytes do not have is refused as a mismatch and
/// leaves nothing of the range in the cache.
#[tokio::test]
async fn a_wrong_digest_is_a_mismatch_and_leaves_nothing() {
    let bench = Bench::new("trees-fetch-mismatch").await;
    let (span, size, _) = blob_of(&bench, 0);
    let wrong = TreeDigest::of(b"other bytes");
    assert_eq!(
        bench.store.fetch_span(&span, size, &wrong).await,
        Err(FetchError::Fill(RangeError::Mismatch))
    );
    assert_eq!(bench.rig.cache.span_of(&span.pack, span.offset, size), None);
    assert_eq!(bench.rig.cache.extents_held(), (0, 0));
    assert_eq!(bench.rig.cache.held(), (0, 0));
    assert_eq!(
        FetchError::Fill(RangeError::Mismatch).to_string(),
        "fill: mismatch"
    );
}

/// Asserts that a span that ends past the end of the pack is refused as past the end, as the
/// cache's own fill refuses it, and that a span of a pack the upstream does not have is the
/// upstream's 404; neither leaves anything in the cache.
#[tokio::test]
async fn a_span_past_the_pack_or_of_no_pack_is_refused_as_the_fill_refuses_it() {
    let bench = Bench::new("trees-fetch-past").await;
    let sha = TreeDigest::of(b"anything");
    let past = FileSpan {
        pack: bench.pack.key.clone(),
        offset: 5500,
    };
    assert_eq!(
        bench.store.fetch_span(&past, 1000, &sha).await,
        Err(FetchError::Fill(RangeError::Past))
    );
    let none = FileSpan {
        pack: "cas/pack/none".to_owned(),
        offset: 0,
    };
    assert_eq!(
        bench.store.fetch_span(&none, 10, &sha).await,
        Err(FetchError::Fill(RangeError::Upstream(404)))
    );
    assert_eq!(bench.rig.cache.extents_held(), (0, 0));
    assert_eq!(bench.rig.cache.held(), (0, 0));
}

/// Asserts that a upstream that fails gives the fill's own error with the upstream's status, for
/// the span alone and for a file opened by path, and that once the upstream is back the same
/// request is answered and the bytes are the blob's.
#[tokio::test]
async fn an_upstream_that_fails_is_its_status_and_the_next_try_fills() {
    let (bench, id) = placed("trees-fetch-down").await;
    let (span, size, sha) = blob_of(&bench, 2);
    bench.rig.upstream.set_down(true);
    assert_eq!(
        bench.store.fetch_span(&span, size, &sha).await,
        Err(FetchError::Fill(RangeError::Upstream(503)))
    );
    assert_eq!(
        bench
            .store
            .open_file(&id, "lib/deep/data")
            .await
            .unwrap_err(),
        FetchError::Fill(RangeError::Upstream(503))
    );
    assert_eq!(bench.rig.cache.span_of(&span.pack, span.offset, size), None);
    bench.rig.upstream.set_down(false);
    let held = bench.store.fetch_span(&span, size, &sha).await.unwrap();
    assert_eq!(
        read_at(&held.path, held.offset, size),
        bench.pack.content(2)
    );
}

/// Asserts that two members of one pack fetched by span are two spans of one cache file: the same
/// path, the offsets of the members, each holding its own bytes.
#[tokio::test]
async fn two_members_of_a_pack_are_two_spans_of_one_file() {
    let bench = Bench::new("trees-fetch-two").await;
    let (first, size_first, sha_first) = blob_of(&bench, 0);
    let (third, size_third, sha_third) = blob_of(&bench, 2);
    let one = bench
        .store
        .fetch_span(&first, size_first, &sha_first)
        .await
        .unwrap();
    let two = bench
        .store
        .fetch_span(&third, size_third, &sha_third)
        .await
        .unwrap();
    assert_eq!(one.path, two.path);
    assert_eq!((one.offset, two.offset), (first.offset, third.offset));
    assert_ne!(one.offset, two.offset);
    assert_eq!(
        read_at(&one.path, one.offset, size_first),
        bench.pack.content(0)
    );
    assert_eq!(
        read_at(&two.path, two.offset, size_third),
        bench.pack.content(2)
    );
    let before = upstream_calls(&bench);
    assert_eq!(
        bench.store.fetch_span(&first, size_first, &sha_first).await,
        Ok(one)
    );
    assert_eq!(upstream_calls(&bench), before);
}

/// The blob `blob` of the pack as `fetch_parts` is asked: offset, size and digest.
fn part_of(bench: &Bench, blob: usize) -> (u64, u64, TreeDigest) {
    let (span, size, sha) = blob_of(bench, blob);
    (span.offset, size, sha)
}

/// Asserts that `fetch_parts` of two blobs of a pack is one GET over them, that after it both
/// spans are held and `span_held` says so while it says no for the blob between them and for a
/// pack the cache has not seen, and that the bytes at each span are the blob's.
#[tokio::test]
async fn parts_are_fetched_in_one_read_and_their_spans_are_held() {
    let bench = Bench::new("trees-fetch-parts").await;
    let (first, third) = (part_of(&bench, 0), part_of(&bench, 2));
    let (_, size, _) = blob_of(&bench, 1);
    let (span, _, _) = blob_of(&bench, 1);
    assert!(!bench.store.span_held(&bench.pack.key, first.0, first.1));
    bench
        .store
        .fetch_parts(&bench.pack.key, &[first, third])
        .await
        .unwrap();
    assert_eq!(bench.rig.calls(Op::Get), 1);
    assert!(bench.store.span_held(&bench.pack.key, first.0, first.1));
    assert!(bench.store.span_held(&bench.pack.key, third.0, third.1));
    assert!(!bench.store.span_held(&span.pack, span.offset, size));
    assert!(!bench.store.span_held("cas/pack/none", 0, 10));
    let held = bench
        .rig
        .cache
        .span_of(&bench.pack.key, third.0, third.1)
        .unwrap();
    assert_eq!(
        read_at(&held.path, held.offset, third.1),
        bench.pack.content(2)
    );
}

/// Asserts that `fetch_parts` with one wrong digest is the fill's mismatch, with the part that was
/// right kept and the wrong one not held.
#[tokio::test]
async fn a_part_with_a_wrong_digest_is_a_mismatch_and_the_other_stays_held() {
    let bench = Bench::new("trees-fetch-parts-wrong").await;
    let (first, third) = (part_of(&bench, 0), part_of(&bench, 2));
    let wrong = (third.0, third.1, TreeDigest::of(b"other bytes"));
    assert_eq!(
        bench
            .store
            .fetch_parts(&bench.pack.key, &[first, wrong])
            .await,
        Err(FetchError::Fill(RangeError::Mismatch))
    );
    assert!(bench.store.span_held(&bench.pack.key, first.0, first.1));
    assert!(!bench.store.span_held(&bench.pack.key, third.0, third.1));
}
