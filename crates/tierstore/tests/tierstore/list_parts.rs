//! `Backend::list_parts`: the call held to the contract of the trait on the in-memory and the
//! directory backend (the S3 backend is held to it too, in `s3.rs`, with the same function), the
//! proxy over them (the parts of the cold tier's upload, never a reserved key's), the fault
//! injector that counts and fails the call, and the cache that passes it on.

use std::sync::Arc;

use bytes::Bytes;
use futures::StreamExt;
use futures::stream;
use tierstore::{
    Backend, CachedStore, DirBackend, Error, MemoryBackend, Op, TestClock, UploadedPart,
};
use tierstore_testkit::dir::TestDir;

use crate::support::Rig;

/// Stores `text` as the part `number` of the upload `id` of `key`, and gives its tag.
async fn put(
    backend: &dyn Backend,
    key: &str,
    id: &str,
    number: u32,
    text: &'static str,
) -> String {
    let items: Vec<Result<Bytes, Error>> = vec![Ok(Bytes::from_static(text.as_bytes()))];
    let len = Some(text.len() as u64);
    let stored = backend.upload_part_with_len(key, id, number, len, stream::iter(items).boxed());
    stored.await.unwrap()
}

/// The numbers and sizes of some parts.
fn shape(parts: &[UploadedPart]) -> Vec<(u32, u64)> {
    parts.iter().map(|p| (p.number, p.size)).collect()
}

/// Asserts that a backend lists the parts of an open upload as the trait says: in ascending order
/// of number whatever the order they were stored in, each with the tag the store of the part
/// gave and its size; those above a number only; at most the number asked for; none for a size of
/// zero; one entry for a number stored twice, with the part that stays; and `NotFound` for an
/// upload that is not open, one of another key, and one that was aborted or completed. It catches
/// a listing that is not in order, ignores the marker or the limit, lists a replaced part twice
/// or keeps the first, or lists the parts of an upload that ended.
pub async fn contract(backend: &dyn Backend) {
    let id = backend.create_upload("k").await.unwrap();
    let empty = backend.list_parts("k", &id, None, 10).await.unwrap();
    assert!(empty.is_empty());
    let three = put(backend, "k", &id, 3, "ccc").await;
    let one = put(backend, "k", &id, 1, "a").await;
    let two = put(backend, "k", &id, 2, "bb").await;

    let all = backend.list_parts("k", &id, None, 10).await.unwrap();
    assert_eq!(shape(&all), [(1, 1), (2, 2), (3, 3)]);
    let tags: Vec<&str> = all.iter().map(|part| part.tag.as_str()).collect();
    assert_eq!(tags, [one.as_str(), two.as_str(), three.as_str()]);

    let page = |after: Option<u32>, max: usize| {
        let (id, backend) = (id.clone(), &backend);
        async move { backend.list_parts("k", &id, after, max).await.unwrap() }
    };
    assert_eq!(page(Some(0), 10).await, all);
    assert_eq!(page(Some(1), 10).await, all[1..]);
    assert_eq!(page(Some(2), 10).await, all[2..]);
    assert!(page(Some(3), 10).await.is_empty());
    assert!(page(Some(40), 10).await.is_empty());
    assert_eq!(page(None, 2).await, all[..2]);
    assert_eq!(page(Some(1), 1).await, all[1..2]);
    assert!(page(None, 0).await.is_empty());

    let again = put(backend, "k", &id, 2, "zzzz").await;
    assert_ne!(again, two);
    let replaced = page(Some(1), 10).await;
    assert_eq!(shape(&replaced), [(2, 4), (3, 3)]);
    assert_eq!(replaced[0].tag, again);

    let other = backend.create_upload("other").await.unwrap();
    for (key, id) in [
        ("k", "no-such-upload"),
        ("other", id.as_str()),
        ("k", &other),
    ] {
        let missing = backend.list_parts(key, id, None, 10).await;
        assert!(matches!(missing, Err(Error::NotFound(_))), "{key} {id}");
    }
    backend.abort_upload("k", &id).await.unwrap();
    let aborted = backend.list_parts("k", &id, None, 10).await;
    assert!(matches!(aborted, Err(Error::NotFound(_))));

    let kept = put(backend, "other", &other, 1, "x").await;
    let listed = backend.list_parts("other", &other, None, 10).await;
    assert_eq!(listed.unwrap()[0].tag, kept);
    backend
        .complete_upload("other", &other, &[(1, kept)])
        .await
        .unwrap();
    let completed = backend.list_parts("other", &other, None, 10).await;
    assert!(matches!(completed, Err(Error::NotFound(_))));
}

/// Asserts that the in-memory backend lists the parts of an upload as the trait says, and stamps
/// each with the time of its clock when it was stored (a part stored again with the time of the
/// second store).
#[tokio::test]
async fn the_memory_backend_lists_the_parts_of_an_open_upload() {
    let clock = Arc::new(TestClock::new());
    let backend = MemoryBackend::with_clock(Arc::clone(&clock));
    let id = backend.create_upload("k").await.unwrap();
    clock.set(1_000);
    put(&backend, "k", &id, 1, "a").await;
    clock.set(2_500);
    put(&backend, "k", &id, 2, "bb").await;
    let listed = backend.list_parts("k", &id, None, 10).await.unwrap();
    let times: Vec<u64> = listed.iter().map(|part| part.modified).collect();
    assert_eq!(times, [1_000, 2_500]);
    clock.set(4_000);
    put(&backend, "k", &id, 1, "c").await;
    let again = backend.list_parts("k", &id, Some(0), 1).await.unwrap();
    assert_eq!(again[0].modified, 4_000);
    contract(&MemoryBackend::new()).await;
}

/// Asserts that the directory backend lists the parts of an upload as the trait says, that each
/// carries a size and a time, that a name in the upload's directory that is a part's name with no
/// file behind it (a part being replaced or removed) is not listed, and that a backend opened
/// over the same directory lists the same parts.
#[tokio::test]
async fn the_directory_backend_lists_the_parts_of_an_open_upload() {
    let dir = TestDir::under(env!("CARGO_TARGET_TMPDIR"), "list-parts-dir");
    let backend = DirBackend::open(dir.path()).unwrap();
    contract(&backend).await;
    let id = backend.create_upload("kept").await.unwrap();
    put(&backend, "kept", &id, 1, "abc").await;
    let listed = backend.list_parts("kept", &id, None, 10).await.unwrap();
    assert_eq!(shape(&listed), [(1, 3)]);
    assert!(listed[0].modified > 0, "{listed:?}");
    let upload = dir.path().join(".uploads").join(&id);
    let gone = format!("00002-{}", "0".repeat(64));
    std::os::unix::fs::symlink(upload.join("missing"), upload.join(gone)).unwrap();
    let with_stray = backend.list_parts("kept", &id, None, 10).await.unwrap();
    assert_eq!(with_stray, listed);
    let reopened = DirBackend::open(dir.path()).unwrap();
    let again = reopened.list_parts("kept", &id, None, 10).await.unwrap();
    assert_eq!(again, listed);
}

/// Asserts that the proxy lists the parts of an upload from the tier its id names: the hot tier
/// for an upload made through it with room there, the cold tier for one another writer started in
/// the cold tier, each asking only that tier, and that it refuses the keys it keeps for itself
/// without asking either.
#[tokio::test]
async fn the_proxy_lists_the_parts_of_the_upload_from_the_tier_its_id_names() {
    let rig = Rig::new();
    let proxy = rig.proxy("p1", 1 << 20).await;
    let through = proxy.create_upload("jobs/a").await.unwrap();
    let tag = put(&proxy, "jobs/a", &through, 1, "abc").await;
    let direct = rig.cold_mem.create_upload("jobs/b").await.unwrap();
    put(rig.cold_mem.as_ref(), "jobs/b", &direct, 2, "de").await;

    let first = proxy
        .list_parts("jobs/a", &through, None, 10)
        .await
        .unwrap();
    assert_eq!(shape(&first), [(1, 3)]);
    assert_eq!(first[0].tag, tag);
    let second = proxy.list_parts("jobs/b", &direct, None, 10).await.unwrap();
    assert_eq!(shape(&second), [(2, 2)]);
    // The upload made through the proxy has room in the hot tier and is there; the other is the
    // cold tier's own. Each is asked of the tier its id names, and of that one only.
    assert_eq!(rig.hot.calls(Op::ListParts), 1);
    assert_eq!(rig.cold.calls(Op::ListParts), 1);

    let before = (rig.hot.calls(Op::ListParts), rig.cold.calls(Op::ListParts));
    for key in ["_journal/p1/0", "_tiering/lease", "_entries/x"] {
        let refused = proxy.list_parts(key, &through, None, 10).await;
        assert!(matches!(refused, Err(Error::Reserved(_))), "{key}");
    }
    assert_eq!(
        (rig.hot.calls(Op::ListParts), rig.cold.calls(Op::ListParts)),
        before
    );
    let unknown = proxy.list_parts("jobs/a", "nope", None, 10).await;
    assert!(matches!(unknown, Err(Error::NotFound(_))));
}

/// Asserts that the fault injector counts the listing as a call of its own kind, fails it where
/// told to, and passes the answer on, and that the proxy gives the failure of the cold tier as it
/// is.
#[tokio::test]
async fn the_listing_of_parts_can_be_counted_and_failed() {
    let rig = Rig::new();
    let cold = rig.cold.clone();
    let id = cold.create_upload("k").await.unwrap();
    put(cold.as_ref(), "k", &id, 1, "x").await;
    cold.fail_next(Op::ListParts, 1);
    let down = cold.list_parts("k", &id, None, 10).await.unwrap_err();
    assert!(down.is_unavailable());
    let listed = cold.list_parts("k", &id, None, 10).await.unwrap();
    assert_eq!(shape(&listed), [(1, 1)]);
    assert_eq!(cold.calls(Op::ListParts), 2);

    let proxy = rig.proxy("p1", 1 << 20).await;
    cold.fail_next(Op::ListParts, 1);
    let failed = proxy.list_parts("k", &id, None, 10).await.unwrap_err();
    assert!(failed.is_unavailable());
    let again = proxy.list_parts("k", &id, None, 10).await.unwrap();
    assert_eq!(again, listed);
}

/// Asserts that the read cache passes the listing of parts on to the backend it is in front of.
#[tokio::test]
async fn the_cache_passes_the_listing_of_parts_on() {
    let inner = Arc::new(MemoryBackend::new());
    let cache = CachedStore::new(inner.clone(), 1 << 20);
    let id = inner.create_upload("k").await.unwrap();
    put(inner.as_ref(), "k", &id, 1, "xy").await;
    let listed = cache.list_parts("k", &id, None, 10).await.unwrap();
    assert_eq!(shape(&listed), [(1, 2)]);
    let past = cache.list_parts("k", &id, Some(1), 10).await.unwrap();
    assert!(past.is_empty());
}
