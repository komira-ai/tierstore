//! A directory shared by this branch and the release before it: what one wrote the other reads,
//! and replaces. The release before is the real `tierstore` library of its commit
//! (`tierstore_prev`), not a copy of its format.

use bytes::Bytes;
use futures::{StreamExt, stream};
use tierstore::{Backend, ByteRange, DirBackend, PartBody};
use tierstore_compat::{CHUNK, chunked, patterned};
use tierstore_prev::{
    Backend as PrevBackend, ByteRange as PrevRange, DirBackend as PrevDir, Error as PrevError,
    PartBody as PrevBody,
};
use tierstore_testkit::dir::TestDir;

/// This branch's chunks of a body, as a stream.
fn now_body(seed: u8, len: usize) -> PartBody<'static> {
    let items: Vec<Result<Bytes, tierstore::Error>> = chunked(seed, len)
        .into_iter()
        .map(|chunk| Ok(Bytes::from(chunk)))
        .collect();
    stream::iter(items).boxed()
}

/// The previous release's chunks of a body, as a stream.
fn before_body(seed: u8, len: usize) -> PrevBody<'static> {
    let items: Vec<Result<Bytes, PrevError>> = chunked(seed, len)
        .into_iter()
        .map(|chunk| Ok(Bytes::from(chunk)))
        .collect();
    stream::iter(items).boxed()
}

/// The keys and sizes of the objects each release lists, which must be the same.
async fn listings(now: &DirBackend, before: &PrevDir) -> (Vec<(String, u64)>, Vec<(String, u64)>) {
    let now = now.list("").await.unwrap();
    let before = before.list("").await.unwrap();
    let now = now.into_iter().map(|o| (o.key, o.size)).collect();
    let before = before.into_iter().map(|o| (o.key, o.size)).collect();
    (now, before)
}

/// Asserts that the previous release reads a directory in which this branch wrote a plain object,
/// two reservations (one renewed by the marked replace), an object replaced by a streamed
/// conditional replacement and a streamed object: each has the size, the content tag, the bytes,
/// a range of the bytes and the listing that this branch gives it; a reservation reads as a plain
/// object; the previous release replaces a reservation by the tag, and this branch then reads an
/// object that is no reservation, and deletes an object, and this branch finds it gone.
#[tokio::test]
async fn the_previous_release_reads_and_replaces_what_this_branch_wrote_to_a_directory() {
    let dir = TestDir::under(env!("CARGO_TARGET_TMPDIR"), "compat-dir-new");
    let now = DirBackend::open(dir.path()).unwrap();
    let before = PrevDir::open(dir.path()).unwrap();

    let body = |seed, len| Bytes::from(patterned(seed, len));
    now.put("plain", body(1, 3 * CHUNK + 5)).await.unwrap();
    let held = now.put_reservation("held", body(2, 56)).await.unwrap();
    let first = now.put_reservation("renewed", body(3, 56)).await.unwrap();
    let marked = now.put_reservation_if_match("renewed", body(4, 56), &first);
    marked.await.unwrap();
    now.put("replaced", body(5, 10)).await.unwrap();
    let old = now.head("replaced").await.unwrap().tag;
    let len = Some(2 * CHUNK as u64 + 7);
    let streamed = now.put_stream_if_match("replaced", len, now_body(6, 2 * CHUNK + 7), &old);
    streamed.await.unwrap();
    now.put_stream("streamed", None, now_body(7, CHUNK + 1))
        .await
        .unwrap();

    for key in ["plain", "held", "renewed", "replaced", "streamed"] {
        let seen = now.head(key).await.unwrap();
        let read = before.head(key).await.unwrap();
        assert_eq!((read.size, &read.tag), (seen.size, &seen.tag), "{key}");
        let whole = before.get(key, None).await.unwrap();
        assert_eq!(whole, now.get(key, None).await.unwrap(), "{key}");
        let part = before.get(key, Some(PrevRange::new(3, 20))).await.unwrap();
        assert_eq!(part, whole.slice(3..23), "{key}");
    }
    let (seen, read) = listings(&now, &before).await;
    assert_eq!(seen, read);
    assert_eq!(seen.len(), 5);
    assert!(now.head("held").await.unwrap().reservation);

    let real = Bytes::from_static(b"a real object");
    let swapped = before
        .put_if_match("held", real.clone(), &held)
        .await
        .unwrap();
    let head = now.head("held").await.unwrap();
    assert_eq!(
        (head.reservation, head.tag.as_str()),
        (false, swapped.as_str())
    );
    let marked = now.get_marked("held", None).await.unwrap();
    assert_eq!(marked, (real, false, swapped.clone()));

    before.delete("plain").await.unwrap();
    let gone = now.head("plain").await;
    assert_eq!(gone, Err(tierstore::Error::NotFound("plain".to_owned())));
}

/// Asserts that this branch reads a directory in which the previous release wrote a plain object, a
/// streamed object, an object it replaced by the tag and an object it made by a multipart upload:
/// each has the size, the content tag, the bytes, a range of the bytes, a streamed read and the
/// listing the previous release gives it, none is a reservation; this branch replaces one of them
/// by the tag, whole and streamed, and the previous release then reads the new bytes.
#[tokio::test]
async fn this_branch_reads_and_replaces_what_the_previous_release_wrote_to_a_directory() {
    let dir = TestDir::under(env!("CARGO_TARGET_TMPDIR"), "compat-dir-old");
    let now = DirBackend::open(dir.path()).unwrap();
    let before = PrevDir::open(dir.path()).unwrap();

    let body = |seed, len| Bytes::from(patterned(seed, len));
    before.put("plain", body(1, 3 * CHUNK + 5)).await.unwrap();
    before
        .put_stream(
            "streamed",
            Some(CHUNK as u64 + 1),
            before_body(2, CHUNK + 1),
        )
        .await
        .unwrap();
    before.put("replaced", body(3, 30)).await.unwrap();
    let old = before.head("replaced").await.unwrap().tag;
    let swapped = before.put_if_match("replaced", body(4, 40), &old);
    swapped.await.unwrap();
    let id = before.create_upload("joined").await.unwrap();
    let part = before.upload_part("joined", &id, 1, before_body(5, CHUNK + 3));
    let tag = part.await.unwrap();
    let parts = [(1, tag)];
    before.complete_upload("joined", &id, &parts).await.unwrap();

    for key in ["plain", "streamed", "replaced", "joined"] {
        let read = before.head(key).await.unwrap();
        let seen = now.head(key).await.unwrap();
        assert_eq!((seen.size, &seen.tag), (read.size, &read.tag), "{key}");
        assert!(!seen.reservation, "{key}");
        let whole = before.get(key, None).await.unwrap();
        assert_eq!(now.get(key, None).await.unwrap(), whole, "{key}");
        let marked = now.get_marked(key, None).await.unwrap();
        assert_eq!(marked, (whole.clone(), false, read.tag.clone()), "{key}");
        let part = now.get(key, Some(ByteRange::new(3, 20))).await.unwrap();
        assert_eq!(part, whole.slice(3..23), "{key}");
        let mut streaming = now.get_stream(key, None).await.unwrap();
        let mut joined = Vec::new();
        while let Some(chunk) = streaming.next().await {
            joined.extend_from_slice(&chunk.unwrap());
        }
        assert_eq!(joined, whole, "{key}");
    }
    let (seen, read) = listings(&now, &before).await;
    assert_eq!(seen, read);
    assert_eq!(seen.len(), 4);

    let current = now.head("plain").await.unwrap().tag;
    let replaced = now.put_if_match("plain", body(8, 9), &current);
    let replaced = replaced.await.unwrap();
    assert_eq!(before.get("plain", None).await.unwrap(), body(8, 9));
    let tag = now.head("streamed").await.unwrap().tag;
    let len = Some(CHUNK as u64 + 2);
    let streamed = now.put_stream_if_match("streamed", len, now_body(9, CHUNK + 2), &tag);
    streamed.await.unwrap();
    let head = before.head("streamed").await.unwrap();
    assert_eq!(head.size, CHUNK as u64 + 2);
    let read = before.get("streamed", None).await.unwrap();
    assert_eq!(read, Bytes::from(patterned(9, CHUNK + 2)));
    assert_eq!(before.head("plain").await.unwrap().tag, replaced);
}
