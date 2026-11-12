//! The provided [`Backend::put_stream_sha256`]: what the checked body lets through and stops, and
//! the answer to a key that exists, over the memory backend and over a backend whose read after a
//! refusal fails.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use async_trait::async_trait;
use bytes::Bytes;
use futures::{StreamExt as _, stream};
use sha2::{Digest as _, Sha256};

use crate::put_sha256::checked;
use crate::{Backend, Error, Listed, MemoryBackend, Meta, PartBody, PutOutcome, ReadBody};
use crate::{ByteRange, TreeDigest};

/// The SHA-256 of `bytes`.
fn sha(bytes: &[u8]) -> TreeDigest {
    TreeDigest::of(bytes)
}

/// A body of these chunks.
fn chunks(parts: &[&[u8]]) -> PartBody<'static> {
    let items: Vec<Result<Bytes, Error>> = parts
        .iter()
        .map(|part| Ok(Bytes::copy_from_slice(part)))
        .collect();
    stream::iter(items).boxed()
}

/// What the memory backend holds under `key`, if anything.
async fn held(backend: &MemoryBackend, key: &str) -> Option<Bytes> {
    backend.get(key, None).await.ok()
}

/// The items a checked body gives, each an `Ok` chunk's bytes or an error.
async fn drained(body: PartBody<'_>) -> Vec<Result<Bytes, Error>> {
    body.collect().await
}

fn bad(key: &str) -> Error {
    Error::BadDigest(key.to_owned())
}

/// Asserts that a put of a key that is free makes the object (`Created`), whatever chunks the
/// body comes in, and that the same put sent again finds it and is `Existed`, a success that
/// writes nothing.
#[tokio::test]
async fn a_put_makes_the_object_and_the_same_put_again_finds_it() {
    let backend = MemoryBackend::new();
    let body = chunks(&[b"hello ", b"world"]);
    let made = backend.put_stream_sha256("k", 11, body, sha(b"hello world"));
    assert_eq!(made.await.unwrap(), PutOutcome::Created);
    assert_eq!(held(&backend, "k").await.unwrap(), "hello world");
    let again = chunks(&[b"hello world"]);
    let found = backend.put_stream_sha256("k", 11, again, sha(b"hello world"));
    assert_eq!(found.await.unwrap(), PutOutcome::Existed);
}

/// Asserts that a key that holds other bytes is `AlreadyExists` and keeps them: bytes of the same
/// length, fewer bytes and more bytes (the read of the object stops past the length).
#[tokio::test]
async fn a_key_that_holds_other_bytes_is_already_exists() {
    let backend = MemoryBackend::new();
    for (key, there) in [("same", &b"abc"[..]), ("short", b"ab"), ("long", b"abcd")] {
        backend
            .put(key, Bytes::copy_from_slice(there))
            .await
            .unwrap();
        let put = backend.put_stream_sha256(key, 3, chunks(&[b"xyz"]), sha(b"xyz"));
        let refused = put.await.unwrap_err();
        assert_eq!(refused, Error::AlreadyExists(key.to_owned()), "{key}");
        assert_eq!(held(&backend, key).await.unwrap(), there, "{key}");
    }
}

/// Asserts that a body that is not the digest named, is shorter than the length named, or runs
/// past it, is `BadDigest` and leaves no object.
#[tokio::test]
async fn a_body_that_is_not_its_digest_or_its_length_leaves_nothing() {
    let backend = MemoryBackend::new();
    let cases: [(&str, u64, &[&[u8]], TreeDigest); 3] = [
        ("other", 3, &[b"abc"], sha(b"abd")),
        ("short", 4, &[b"ab", b"c"], sha(b"abc")),
        ("long", 2, &[b"ab", b"c"], sha(b"ab")),
    ];
    for (key, len, parts, digest) in cases {
        let put = backend.put_stream_sha256(key, len, chunks(parts), digest);
        assert_eq!(put.await.unwrap_err(), bad(key), "{key}");
        assert_eq!(held(&backend, key).await, None, "{key}");
    }
}

/// Asserts the bounds of the length: an empty object is made when the digest named is that of no
/// bytes and refused when it is not, and an empty body for a length of one is short.
#[tokio::test]
async fn an_empty_object_is_checked_like_any_other() {
    let backend = MemoryBackend::new();
    let empty = backend.put_stream_sha256("e", 0, chunks(&[]), sha(b""));
    assert_eq!(empty.await.unwrap(), PutOutcome::Created);
    assert_eq!(held(&backend, "e").await.unwrap(), "");
    let wrong = backend.put_stream_sha256("w", 0, chunks(&[]), sha(b"x"));
    assert_eq!(wrong.await.unwrap_err(), bad("w"));
    let short = backend.put_stream_sha256("s", 1, chunks(&[b""]), sha(b"x"));
    assert_eq!(short.await.unwrap_err(), bad("s"));
    assert_eq!(held(&backend, "w").await, None);
    assert_eq!(held(&backend, "s").await, None);
}

/// Asserts that the checked body gives the chunk that completes the length only when the whole
/// is the digest, an error in its place when it is not, and nothing after an error: the chunk
/// after the bad one and the end of the body are never polled.
#[tokio::test]
async fn the_check_is_made_in_place_of_the_chunk_that_completes_the_length() {
    let good = drained(checked("k", 3, sha(b"abc"), chunks(&[b"a", b"bc"]))).await;
    let good: Vec<Bytes> = good.into_iter().map(Result::unwrap).collect();
    assert_eq!(good, [Bytes::from_static(b"a"), Bytes::from_static(b"bc")]);
    // An empty chunk after the length is complete is passed on: it adds no byte.
    let trailing = drained(checked("k", 3, sha(b"abc"), chunks(&[b"abc", b""]))).await;
    assert!(trailing.iter().all(Result::is_ok), "{trailing:?}");
    assert_eq!(trailing.len(), 2);

    let polled = Arc::new(AtomicUsize::new(0));
    let counted = Arc::clone(&polled);
    let body = chunks(&[b"a", b"bd", b"more"])
        .inspect(move |_| {
            counted.fetch_add(1, Ordering::SeqCst);
        })
        .boxed();
    let items = drained(checked("k", 3, sha(b"abc"), body)).await;
    assert_eq!(items.len(), 2);
    assert_eq!(items[0].as_ref().unwrap(), "a");
    assert_eq!(items[1].as_ref().unwrap_err(), &bad("k"));
    assert_eq!(
        polled.load(Ordering::SeqCst),
        2,
        "nothing is polled after the error"
    );
}

/// Asserts that an error of the body itself is passed on as it is, ends the body, and leaves no
/// object.
#[tokio::test]
async fn an_error_of_the_body_is_passed_on_and_leaves_nothing() {
    let cut = Error::Unavailable("the writer went away".to_owned());
    let items = vec![Ok(Bytes::from_static(b"ab")), Err(cut.clone())];
    let body: PartBody<'static> = stream::iter(items).boxed();
    let backend = MemoryBackend::new();
    let put = backend.put_stream_sha256("k", 4, body, sha(b"abcd"));
    assert_eq!(put.await.unwrap_err(), cut);
    assert_eq!(held(&backend, "k").await, None);
}

/// A backend that refuses every put as `AlreadyExists` and whose read of the object then fails
/// as `read` says: none, the object is gone; one, its body ends with that error after a byte.
struct Refusing {
    read: Option<Error>,
    open: Option<Error>,
}

#[async_trait]
impl Backend for Refusing {
    async fn put(&self, key: &str, _body: Bytes) -> Result<(), Error> {
        Err(Error::AlreadyExists(key.to_owned()))
    }

    async fn put_if_match(&self, _key: &str, _body: Bytes, _etag: &str) -> Result<String, Error> {
        Err(Error::Unsupported("not here".to_owned()))
    }

    async fn get(&self, key: &str, _range: Option<ByteRange>) -> Result<Bytes, Error> {
        Err(Error::NotFound(key.to_owned()))
    }

    async fn get_stream(&self, key: &str, _range: Option<ByteRange>) -> Result<ReadBody, Error> {
        if let Some(error) = &self.open {
            return Err(error.clone());
        }
        let Some(error) = &self.read else {
            return Err(Error::NotFound(key.to_owned()));
        };
        let items = vec![Ok(Bytes::from_static(b"a")), Err(error.clone())];
        Ok(stream::iter(items).boxed())
    }

    async fn head(&self, key: &str) -> Result<Meta, Error> {
        Err(Error::NotFound(key.to_owned()))
    }

    async fn delete(&self, _key: &str) -> Result<(), Error> {
        Ok(())
    }

    async fn list(&self, _prefix: &str) -> Result<Vec<Listed>, Error> {
        Ok(Vec::new())
    }
}

/// Asserts what a refusal whose object cannot be compared answers: an object gone by the time it
/// is read is a `Conflict` (a retry gets the true answer), and a read that fails, at its start or
/// in its body, gives that failure, never `Existed`.
#[tokio::test]
async fn a_refusal_whose_object_cannot_be_read_is_not_a_success() {
    let down = Error::Unavailable("down".to_owned());
    let gone = Refusing {
        read: None,
        open: None,
    };
    let put = gone.put_stream_sha256("k", 1, chunks(&[b"a"]), sha(b"a"));
    assert_eq!(put.await.unwrap_err(), Error::Conflict("k".to_owned()));
    let failing = Refusing {
        read: Some(down.clone()),
        open: None,
    };
    let put = failing.put_stream_sha256("k", 1, chunks(&[b"a"]), sha(b"a"));
    assert_eq!(put.await.unwrap_err(), down);
    let unopened = Refusing {
        read: None,
        open: Some(down.clone()),
    };
    let put = unopened.put_stream_sha256("k", 1, chunks(&[b"a"]), sha(b"a"));
    assert_eq!(put.await.unwrap_err(), down);
}

/// Asserts that the digest compared is the SHA-256 of the bytes, by a known vector.
#[test]
fn the_digest_is_sha256() {
    let known = "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad";
    assert_eq!(sha(b"abc").to_string(), known);
    let direct: [u8; 32] = Sha256::digest(b"abc").into();
    assert_eq!(sha(b"abc").as_bytes(), &direct);
}
