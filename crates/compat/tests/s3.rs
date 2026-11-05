//! One S3 service (the in-process loopback service of `objstore-s3`) shared by this branch and the
//! release before it: what one wrote, with its user metadata, the other reads, and replaces. The
//! release before is the real `tierstore` library of its commit (`tierstore_prev`), not a copy of
//! its requests.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "an integration test: a failed unwrap or expect is the failure it reports"
)]

use std::time::Duration;

use bytes::Bytes;
use futures::{StreamExt, stream};
use objstore_s3::s3::fake::FakeS3;
use tierstore::s3::{Capabilities, Credentials, S3Config};
use tierstore::{Backend, ByteRange, PartBody, S3Backend};
use tierstore_compat::{CHUNK, chunked, patterned};
use tierstore_prev::s3 as prev_s3;
use tierstore_prev::{
    Backend as PrevBackend, ByteRange as PrevRange, Error as PrevError, PartBody as PrevBody,
    S3Backend as PrevS3,
};

/// Both releases' clients of the service at `endpoint`.
fn both(endpoint: &str) -> (S3Backend, PrevS3) {
    let now = S3Config {
        endpoint: endpoint.to_owned(),
        region: "us-east-1".to_owned(),
        bucket: "tier-test".to_owned(),
        credentials: Credentials::new("test-key", "test-secret"),
        capabilities: Capabilities::default(),
        connect_timeout: Duration::from_secs(5),
        request_timeout: Duration::from_secs(30),
        min_body_rate: None,
    };
    let before = prev_s3::S3Config {
        endpoint: endpoint.to_owned(),
        region: "us-east-1".to_owned(),
        bucket: "tier-test".to_owned(),
        credentials: prev_s3::Credentials::new("test-key", "test-secret"),
        capabilities: prev_s3::Capabilities::default(),
        connect_timeout: Duration::from_secs(5),
        request_timeout: Duration::from_secs(30),
        min_body_rate: None,
    };
    (S3Backend::new(now).unwrap(), PrevS3::new(before).unwrap())
}

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
async fn listings(now: &S3Backend, before: &PrevS3) -> (Vec<(String, u64)>, Vec<(String, u64)>) {
    let now = now.list("").await.unwrap();
    let before = before.list("").await.unwrap();
    let now = now.into_iter().map(|o| (o.key, o.size)).collect();
    let before = before.into_iter().map(|o| (o.key, o.size)).collect();
    (now, before)
}

/// Asserts that the previous release reads a bucket in which this branch wrote two plain objects,
/// an object replaced by a streamed conditional replacement and a streamed object: each has the
/// size, the content tag, the bytes, a range of the bytes and the listing that this branch gives
/// it; the previous release replaces an object by the tag, and this branch then reads the new
/// object and its tag, and deletes an object, and this branch finds it gone.
#[tokio::test]
async fn the_previous_release_reads_and_replaces_what_this_branch_wrote_to_s3() {
    let s3 = FakeS3::start().await;
    let (now, before) = both(s3.endpoint());

    let body = |seed, len| Bytes::from(patterned(seed, len));
    now.put("plain", body(1, 3 * CHUNK + 5)).await.unwrap();
    let held = now.put_tagged("held", body(2, 56)).await.unwrap();
    now.put("replaced", body(5, 10)).await.unwrap();
    let old = now.head("replaced").await.unwrap().tag;
    let len = Some(2 * CHUNK as u64 + 7);
    let streamed = now.put_stream_if_match("replaced", len, now_body(6, 2 * CHUNK + 7), &old);
    streamed.await.unwrap();
    let len = Some(CHUNK as u64 + 1);
    now.put_stream("streamed", len, now_body(7, CHUNK + 1))
        .await
        .unwrap();

    for key in ["plain", "held", "replaced", "streamed"] {
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
    assert_eq!(seen.len(), 4);

    let real = Bytes::from_static(b"a real object");
    let swapped = before
        .put_if_match("held", real.clone(), &held)
        .await
        .unwrap();
    let head = now.head("held").await.unwrap();
    assert_eq!(head.tag, swapped);
    let tagged = now.get_tagged("held", None).await.unwrap();
    assert_eq!(tagged, (real, swapped.clone()));

    before.delete("plain").await.unwrap();
    let gone = now.head("plain").await;
    assert_eq!(gone, Err(tierstore::Error::NotFound("plain".to_owned())));
}

/// Asserts that this branch reads a bucket in which the previous release wrote a plain object, a
/// streamed object, an object it replaced by the tag and an object it made by a multipart upload:
/// each has the size, the content tag, the bytes, a range of the bytes, a streamed read and the
/// listing the previous release gives it; this branch replaces one of
/// them by the tag, whole and streamed, and the previous release then reads the new bytes.
#[tokio::test]
async fn this_branch_reads_and_replaces_what_the_previous_release_wrote_to_s3() {
    let s3 = FakeS3::start().await;
    s3.set_min_part_bytes(0);
    let (now, before) = both(s3.endpoint());

    let body = |seed, len| Bytes::from(patterned(seed, len));
    before.put("plain", body(1, 3 * CHUNK + 5)).await.unwrap();
    let len = Some(CHUNK as u64 + 1);
    let streamed = before.put_stream("streamed", len, before_body(2, CHUNK + 1));
    streamed.await.unwrap();
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
        let whole = before.get(key, None).await.unwrap();
        assert_eq!(now.get(key, None).await.unwrap(), whole, "{key}");
        let tagged = now.get_tagged(key, None).await.unwrap();
        assert_eq!(tagged, (whole.clone(), read.tag.clone()), "{key}");
        let range = Some(ByteRange::new(3, 20));
        let ranged = now.get_tagged(key, range).await.unwrap();
        assert_eq!(ranged, (whole.slice(3..23), read.tag.clone()), "{key}");
        let part = now.get(key, range).await.unwrap();
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
    let tag = now.head("joined").await.unwrap().tag;
    let len = Some(CHUNK as u64 + 2);
    let streamed = now.put_stream_if_match("joined", len, now_body(9, CHUNK + 2), &tag);
    streamed.await.unwrap();
    let head = before.head("joined").await.unwrap();
    assert_eq!(head.size, CHUNK as u64 + 2);
    let read = before.get("joined", None).await.unwrap();
    assert_eq!(read, Bytes::from(patterned(9, CHUNK + 2)));
    assert_eq!(before.head("plain").await.unwrap().tag, replaced);
}

/// Asserts, over one service, what each release does with keys S3's rules take that the previous
/// release's client refused (a space, parentheses, `+`, `%`, non-ASCII text): this branch puts,
/// reads, lists and deletes them; the previous release cannot name them, so a read, a head and
/// a delete of one are `Error::Other` and no request is made, and a listing of a prefix that holds
/// one fails as a whole with an error that names the key (it does not skip the key, and it does not
/// return a page without it), while a listing of a prefix that does not hold one, and the reads
/// of the plain keys beside them, work; once this branch has deleted them the previous release
/// lists the bucket again; and this branch reads and lists what the previous release wrote.
#[tokio::test]
async fn the_previous_release_cannot_name_keys_this_branch_takes_and_its_listings_fail_over_them() {
    let s3 = FakeS3::start().await;
    let (now, before) = both(s3.endpoint());
    let wide = ["wide/a b", "wide/f(x)+1%", "wide/caf\u{e9}"];

    before
        .put("plain/old", Bytes::from_static(b"old"))
        .await
        .unwrap();
    now.put("plain/new", Bytes::from_static(b"new"))
        .await
        .unwrap();
    for key in wide {
        now.put(key, Bytes::from_static(b"wide")).await.unwrap();
        assert_eq!(now.get(key, None).await.unwrap(), "wide", "{key}");
    }
    let listed = now.list("wide/").await.unwrap();
    let mut keys: Vec<&str> = listed.iter().map(|o| o.key.as_str()).collect();
    keys.sort_unstable();
    let mut want = wide.to_vec();
    want.sort_unstable();
    assert_eq!(keys, want);

    let requests = s3.requests().len();
    for key in wide {
        let read = before.get(key, None).await;
        assert!(matches!(read, Err(PrevError::Other(_))), "{key}");
        let head = before.head(key).await;
        assert!(matches!(head, Err(PrevError::Other(_))), "{key}");
        let delete = before.delete(key).await;
        assert!(matches!(delete, Err(PrevError::Other(_))), "{key}");
    }
    assert_eq!(
        s3.requests().len(),
        requests,
        "no request for a key it cannot name"
    );

    for prefix in ["wide/", ""] {
        let refused = before.list(prefix).await.unwrap_err();
        assert!(refused.to_string().contains("listed key"), "{refused}");
    }
    let plain = before.list("plain/").await.unwrap();
    let plain: Vec<&str> = plain.iter().map(|o| o.key.as_str()).collect();
    assert_eq!(plain, ["plain/new", "plain/old"]);
    assert_eq!(before.get("plain/new", None).await.unwrap(), "new");
    assert_eq!(now.get("plain/old", None).await.unwrap(), "old");

    for key in wide {
        now.delete(key).await.unwrap();
    }
    let (seen, read) = listings(&now, &before).await;
    assert_eq!(seen, read);
    assert_eq!(seen.len(), 2);
}
