//! The S3 store's keys follow S3's rules: any text of 1 to 1024 bytes is a key, and a key
//! with `(`, `)`, a space, `+`, `%`, `&`, `<`, `?`, `#` or non-ASCII text goes through every
//! call (put, head, get, list, multipart, delete) against the loopback S3 service of
//! `s3::fake`, which checks the SigV4 signature of each request over the path as sent.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "an integration test: a failed unwrap or expect is the failure it reports"
)]

use bytes::Bytes;
use futures::StreamExt;
use futures::stream;
use objstore_s3::s3::fake::FakeS3;
use objstore_s3::{
    ByteRange, Capabilities, KeyError, KeyPrefix, ListToken, ObjectKey, ObjectStore, PageSize,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

/// Keys S3 accepts that the old character set refused, one kind of difficulty each.
const KEYS: [&str; 17] = [
    "f(x) g(y).txt",
    "plus+sign",
    "100%",
    "a%2Fb",
    "a%41",
    "and&amp;",
    "less<than>",
    "caf\u{e9} \u{65e5}\u{672c}\u{8a9e}",
    "emoji \u{1f600}",
    "what?x=1#frag",
    "sp ace",
    "/leading",
    "trailing/",
    "a//b",
    "tab\there\nline\rbreak",
    "quote\"apos'",
    "...",
];

fn key(name: &str) -> ObjectKey {
    ObjectKey::new(name).unwrap()
}

fn store(s3: &FakeS3) -> objstore_s3::s3::S3Store {
    s3.require_credentials("fake-key", "fake-secret");
    s3.store(Capabilities::default())
}

/// Asserts that each key is put, found by a HEAD, read back whole and by range, listed
/// (by the whole listing and by a prefix that holds the characters), and deleted, and that
/// the service, which verifies every signature, took each request. A path signed unlike it is
/// sent (a bare `%`, `+` or space) is refused 403 by it.
#[tokio::test]
async fn keys_with_s3_characters_round_trip_through_put_head_get_list_and_delete() {
    let s3 = FakeS3::start().await;
    let store = store(&s3);
    for name in KEYS {
        let k = key(name);
        let body = Bytes::from(format!("body of {name}"));
        store
            .put_new(&k, body.clone(), None)
            .await
            .unwrap_or_else(|e| panic!("put {name:?}: {e}"));
        assert_eq!(
            store.len(&k).await.unwrap(),
            Some(body.len() as u64),
            "{name:?}"
        );
        let whole = ByteRange::new(0, body.len() as u64).unwrap();
        assert_eq!(store.get_range(&k, whole).await.unwrap(), body, "{name:?}");
        let tail = ByteRange::new(3, 4).unwrap();
        assert_eq!(store.get_range(&k, tail).await.unwrap(), body.slice(3..7));
    }
    let mut want: Vec<String> = KEYS.iter().map(|k| (*k).to_owned()).collect();
    want.sort();
    assert_eq!(s3.keys(), want, "the service holds the keys as written");

    let page = store
        .list(&KeyPrefix::default(), None, PageSize::MAX)
        .await
        .unwrap();
    let listed: Vec<&str> = page.objects.iter().map(|o| o.key.as_str()).collect();
    assert_eq!(listed, want);
    assert!(page.next.is_none());

    // A prefix with the characters in it travels in the query.
    let prefix = KeyPrefix::new("f(x) g").unwrap();
    let page = store.list(&prefix, None, PageSize::MAX).await.unwrap();
    assert_eq!(page.objects.len(), 1);
    assert_eq!(page.objects[0].key.as_str(), "f(x) g(y).txt");

    for name in KEYS {
        let k = key(name);
        store.delete(&k).await.unwrap();
        assert_eq!(store.len(&k).await.unwrap(), None, "{name:?}");
    }
    assert!(s3.keys().is_empty());
}

/// Asserts that a listing split into pages of one carries every key across the pages: the
/// continuation token the service issues is a key with these characters, sent back in the
/// query, and `start-after` with a key does the same.
#[tokio::test]
async fn paging_resumes_after_a_key_with_s3_characters() {
    let s3 = FakeS3::start().await;
    let store = store(&s3);
    for name in KEYS {
        store
            .put_new(&key(name), Bytes::from_static(b"x"), None)
            .await
            .unwrap();
    }
    let one = PageSize::new(1).unwrap();
    let (mut seen, mut after) = (Vec::new(), None);
    loop {
        let page = store
            .list(&KeyPrefix::default(), after.as_ref(), one)
            .await
            .unwrap();
        seen.extend(page.objects.iter().map(|o| o.key.as_str().to_owned()));
        match page.next {
            Some(next) => after = Some(next),
            None => break,
        }
    }
    let mut want: Vec<String> = KEYS.iter().map(|k| (*k).to_owned()).collect();
    want.sort();
    assert_eq!(seen, want);

    let from = ListToken::after(&key("a%2Fb"));
    let page = store
        .list(&KeyPrefix::new("a").unwrap(), Some(&from), PageSize::MAX)
        .await
        .unwrap();
    let keys: Vec<&str> = page.objects.iter().map(|o| o.key.as_str()).collect();
    assert_eq!(keys, ["a%41", "a//b", "and&amp;"]);
}

/// Asserts that a multipart upload of a key with these characters is created, takes parts,
/// is listed among the open uploads under the key as written (paged one at a time, so the
/// next-key marker with the characters is read back), completes into an object that reads back,
/// and that an upload of such a key can be aborted.
#[tokio::test]
async fn multipart_uploads_of_keys_with_s3_characters_complete_list_and_abort() {
    let s3 = FakeS3::start().await;
    let store = store(&s3);
    let names = ["f(x) g+h&i<j>%k", "caf\u{e9} #1?"];
    let mut open = Vec::new();
    for name in names {
        let k = key(name);
        let id = store.create_multipart(&k).await.unwrap();
        let part = Bytes::from(format!("part of {name}"));
        let body = stream::iter([Ok(part.clone())]).boxed();
        let etag = store
            .upload_part_stream(&k, &id, 1, part.len() as u64, body)
            .await
            .unwrap();
        open.push((k, id, etag, part));
    }
    let one = PageSize::new(1).unwrap();
    let first = store
        .list_multipart_uploads(&KeyPrefix::default(), None, one)
        .await
        .unwrap();
    let (next_key, next_id) = first.next.clone().expect("a second page");
    let second = store
        .list_multipart_uploads(
            &KeyPrefix::default(),
            Some((&next_key, Some(next_id.as_str()))),
            one,
        )
        .await
        .unwrap();
    let mut listed: Vec<&str> = first
        .uploads
        .iter()
        .chain(&second.uploads)
        .map(|u| u.key.as_str())
        .collect();
    listed.sort_unstable();
    let mut want = names.to_vec();
    want.sort_unstable();
    assert_eq!(listed, want);

    let (k, id, etag, part) = &open[0];
    store
        .complete_multipart(k, id, &[(1, etag.clone())])
        .await
        .unwrap();
    let whole = ByteRange::new(0, part.len() as u64).unwrap();
    assert_eq!(&store.get_range(k, whole).await.unwrap(), part);

    let (k, id, _, _) = &open[1];
    store.abort_multipart(k, id).await.unwrap();
    assert_eq!(s3.incomplete_uploads(), 0);
    assert_eq!(store.len(k).await.unwrap(), None);
}

/// Asserts the path of the request: each byte but `A-Za-z0-9 - _ . ~` and the `/` between
/// segments is percent-encoded once (`%` itself as `%25`), so what is signed is what is sent
/// and what the service decodes is the key.
#[tokio::test]
async fn the_request_path_is_the_key_percent_encoded_once() {
    let s3 = FakeS3::start().await;
    let store = store(&s3);
    let k = key("d/a b+c%d&e/caf\u{e9}(1)?#");
    store
        .put_new(&k, Bytes::from_static(b"x"), None)
        .await
        .unwrap();
    assert_eq!(
        s3.requests(),
        ["PUT /tierstore-fake/d/a%20b%2Bc%25d%26e/caf%C3%A9%281%29%3F%23?"]
    );
}

/// Asserts the length limit: a key of 1024 bytes is put and read back, a key of 1025 bytes
/// is refused before any request is sent ([`KeyError::TooLong`]), and a service that is sent
/// one anyway answers 400 `KeyTooLongError`, as S3 does.
#[tokio::test]
async fn a_key_of_1024_bytes_is_accepted_and_one_of_1025_is_refused() {
    let s3 = FakeS3::start().await;
    let store = store(&s3);
    let long = key(&format!("{}\u{e9}", "k".repeat(1022)));
    assert_eq!(long.as_str().len(), 1024);
    store
        .put_new(&long, Bytes::from_static(b"x"), None)
        .await
        .unwrap();
    assert_eq!(store.len(&long).await.unwrap(), Some(1));
    assert_eq!(
        ObjectKey::new("k".repeat(1025)),
        Err(KeyError::TooLong(1025))
    );

    let addr = s3.endpoint().trim_start_matches("http://").to_owned();
    let mut sock = TcpStream::connect(&addr).await.unwrap();
    let request = format!(
        "PUT /tierstore-fake/{} HTTP/1.1\r\nhost: {addr}\r\ncontent-length: 0\r\n\r\n",
        "k".repeat(1025)
    );
    sock.write_all(request.as_bytes()).await.unwrap();
    // The service keeps the connection open: read until the error document ends.
    let mut got = String::new();
    let mut chunk = [0_u8; 1024];
    while !got.contains("</Error>") {
        let n = sock.read(&mut chunk).await.unwrap();
        assert!(n > 0, "closed before the error document: {got}");
        got.push_str(&String::from_utf8_lossy(&chunk[..n]));
    }
    assert!(got.starts_with("HTTP/1.1 400 Bad Request"), "{got}");
    assert!(got.contains("<Code>KeyTooLongError</Code>"), "{got}");
    assert_eq!(s3.keys().len(), 1);
}

/// Asserts that the segments the HTTP client would rewrite are refused as keys (`.` and `..`
/// as a whole segment), and that names holding dots, which it leaves alone, are put and read.
#[tokio::test]
async fn dot_segments_are_refused_and_dotted_names_are_not() {
    for bad in ["a/../b", "./a", "a/.", ".."] {
        assert!(matches!(ObjectKey::new(bad), Err(KeyError::Segment(_))));
    }
    let s3 = FakeS3::start().await;
    let store = store(&s3);
    for name in ["a/..b/.c/d..", "...", ".hidden"] {
        let k = key(name);
        store
            .put_new(&k, Bytes::from_static(b"x"), None)
            .await
            .unwrap();
        assert_eq!(store.len(&k).await.unwrap(), Some(1), "{name}");
    }
}
