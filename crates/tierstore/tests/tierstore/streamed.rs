//! Streamed reads and streamed conditional replacements: the memory and the directory backend held
//! to the checks every backend passes, and the wrappers that pass the calls on (the fault
//! injector, the RAM cache). The S3 backend's are in `s3_streamed.rs`, the directory backend's
//! chunking in its own unit tests.

use std::sync::Arc;

use tierstore::{Backend, CachedStore, DirBackend, Error, FaultyBackend, MemoryBackend, Op};
use tierstore_testkit::dir::TestDir;

use crate::conformance;
use crate::support::{body, chunks};

/// Runs a check over a backend of each kind that needs no service, each from empty.
macro_rules! over_each {
    ($check:path) => {{
        $check(&MemoryBackend::new()).await;
        let dir = TestDir::under(env!("CARGO_TARGET_TMPDIR"), "streamed-dir");
        $check(&DirBackend::open(dir.path()).unwrap()).await;
    }};
}

/// Asserts that a streamed read gives the bytes of the object or the range, the same a get gives,
/// with an empty range and a range at the end giving none, a range past the end an error and an
/// absent key `NotFound`, on the in-memory and the directory backend.
#[tokio::test]
async fn a_streamed_read_gives_the_bytes_of_its_range() {
    over_each!(conformance::a_streamed_read_gives_the_bytes_of_its_range);
}

/// Asserts that a streamed replacement of a reservation takes effect only on the tag and a body
/// that ended whole, and then gives an object that is no reservation, on the in-memory and the
/// directory backend.
#[tokio::test]
async fn a_streamed_replace_takes_the_tag_and_a_whole_body() {
    over_each!(conformance::a_streamed_replace_takes_the_tag_and_a_whole_body);
}

/// Asserts that the fault injector counts a streamed replacement as a call of kind `PutIfMatch`,
/// passes it on, and fails the next one when told to, leaving the object as it was.
#[tokio::test]
async fn an_injector_counts_and_fails_a_streamed_replace_as_a_put_if_match() {
    let backend = FaultyBackend::new(Arc::new(MemoryBackend::new()));
    backend.put("k", body("old")).await.unwrap();
    let tag = backend.head("k").await.unwrap().tag;
    let replaced = backend.put_stream_if_match("k", None, chunks(&["ne", "w"]), &tag);
    let new = replaced.await.unwrap();
    assert_eq!(backend.calls(Op::PutIfMatch), 1);
    assert_eq!(backend.calls(Op::Put), 1);
    assert_eq!(backend.get("k", None).await.unwrap(), body("new"));

    backend.fail_next(Op::PutIfMatch, 1);
    let failed = backend.put_stream_if_match("k", None, chunks(&["x"]), &new);
    let failed = failed.await.unwrap_err();
    assert!(failed.is_unavailable(), "{failed}");
    assert_eq!(backend.calls(Op::PutIfMatch), 2);
    assert_eq!(backend.head("k").await.unwrap().tag, new);
}

/// Asserts that a RAM cache passes a streamed replacement on and forgets what it had cached of the
/// key, so that a read after it gives the new bytes, and that a refused one changes nothing.
#[tokio::test]
async fn a_cache_passes_a_streamed_replace_on_and_forgets_the_key() {
    let cache = CachedStore::new(Arc::new(MemoryBackend::new()), 1 << 20);
    cache.put("k", body("old")).await.unwrap();
    assert_eq!(cache.get("k", None).await.unwrap(), body("old"));
    assert_eq!(cache.cached_bytes(), 3);
    let tag = cache.head("k").await.unwrap().tag;

    let replaced = cache.put_stream_if_match("k", None, chunks(&["ne", "w"]), &tag);
    let new = replaced.await.unwrap();
    assert_eq!(cache.cached_bytes(), 0);
    assert_eq!(cache.get("k", None).await.unwrap(), body("new"));
    let stale = cache
        .put_stream_if_match("k", None, chunks(&["x"]), &tag)
        .await;
    assert_eq!(stale, Err(Error::PreconditionFailed("k".to_owned())));
    assert_eq!(cache.cached_bytes(), 3);
    assert_eq!(cache.head("k").await.unwrap().tag, new);
}
