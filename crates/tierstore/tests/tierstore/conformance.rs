//! What every [`Backend`] does, as checks over any backend: the tests of the in-memory
//! backend call each of them, and, with the `s3` feature, so do those of the S3 backend over
//! the loopback S3 service. A check starts from an empty backend.

use bytes::Bytes;
use futures::StreamExt;
use tierstore::{Backend, ByteRange, Error, Opened, Span, Window};

use crate::support::{body, broken, check_ranges, chunks, keys, page};

/// A second put of a key is refused with `AlreadyExists`, and the first object stays.
pub async fn a_second_put_is_refused(backend: &dyn Backend) {
    backend.put("k", body("one")).await.unwrap();
    let again = backend.put("k", body("two")).await;
    assert_eq!(again, Err(Error::AlreadyExists("k".to_owned())));
    assert_eq!(backend.get("k", None).await.unwrap(), body("one"));
}

/// A tagged put gives the tag a HEAD of the object gives (the tag of the object it created),
/// refuses a second put of the key with `AlreadyExists`, and leaves the first object and its tag.
pub async fn a_tagged_put_gives_the_tag_a_head_gives(backend: &dyn Backend) {
    let tag = backend.put_tagged("k", body("one")).await.unwrap();
    assert_eq!(backend.head("k").await.unwrap().tag, tag);
    let again = backend.put_tagged("k", body("two")).await;
    assert_eq!(again, Err(Error::AlreadyExists("k".to_owned())));
    assert_eq!(backend.get("k", None).await.unwrap(), body("one"));
    assert_eq!(backend.head("k").await.unwrap().tag, tag);
}

/// Ranges of an object are byte-exact.
pub async fn ranges_are_byte_exact(backend: &dyn Backend) {
    backend.put("k", body("0123456789")).await.unwrap();
    check_ranges(backend, "k").await;
}

/// Head gives the size and a content tag that is equal for equal bytes and different for
/// different bytes of the same size.
pub async fn head_gives_the_size_and_a_tag_of_the_content(backend: &dyn Backend) {
    backend.put("a", body("same")).await.unwrap();
    backend.put("b", body("same")).await.unwrap();
    backend.put("c", body("diff")).await.unwrap();
    let a = backend.head("a").await.unwrap();
    let b = backend.head("b").await.unwrap();
    let c = backend.head("c").await.unwrap();
    assert_eq!(a.size, 4);
    assert!(a.same_content(&b));
    assert!(!a.same_content(&c));
}

/// An absent key is not found by a get or a head, and deleting it succeeds.
pub async fn an_absent_key_is_not_found_and_deleting_it_succeeds(backend: &dyn Backend) {
    let missing = Error::NotFound("k".to_owned());
    assert_eq!(backend.get("k", None).await.unwrap_err(), missing);
    assert_eq!(backend.head("k").await.unwrap_err(), missing);
    assert_eq!(backend.delete("k").await, Ok(()));
    assert_eq!(keys(backend).await, Vec::<String>::new());
}

/// A delete removes the object, and the key can be put again.
pub async fn a_deleted_key_is_gone_and_can_be_put_again(backend: &dyn Backend) {
    backend.put("k", body("old")).await.unwrap();
    backend.delete("k").await.unwrap();
    assert_eq!(keys(backend).await, Vec::<String>::new());
    backend.put("k", body("new")).await.unwrap();
    assert_eq!(backend.get("k", None).await.unwrap(), body("new"));
}

/// A listing holds the keys under the prefix, in key order, with their sizes.
pub async fn a_listing_holds_the_keys_under_a_prefix_in_order(backend: &dyn Backend) {
    backend.put("b/2", body("bb")).await.unwrap();
    backend.put("a/1", body("a")).await.unwrap();
    backend.put("b/1", body("b")).await.unwrap();
    assert_eq!(keys(backend).await, ["a/1", "b/1", "b/2"]);
    let listed = backend.list("b/").await.unwrap();
    let sizes: Vec<u64> = listed.iter().map(|object| object.size).collect();
    assert_eq!(sizes, [1, 2]);
    let names: Vec<&str> = listed.iter().map(|object| object.key.as_str()).collect();
    assert_eq!(names, ["b/1", "b/2"]);
}

/// The tag of an object in a listing, whole or in a page, is the tag a HEAD of it gives, and
/// differs between objects of different bytes: the front writes it as the `ETag` of the listing.
pub async fn a_listing_gives_the_tag_a_head_gives(backend: &dyn Backend) {
    backend.put("a/1", body("one")).await.unwrap();
    backend.put("a/2", body("two")).await.unwrap();
    let listed = backend.list("a/").await.unwrap();
    let paged = backend.list_page("a/", None, 10).await.unwrap();
    assert_eq!(listed, paged);
    assert_eq!(listed.len(), 2);
    for object in &listed {
        let head = backend.head(&object.key).await.unwrap();
        assert_eq!(object.tag, head.tag, "{}", object.key);
        assert!(!object.tag.is_empty());
    }
    assert_ne!(listed[0].tag, listed[1].tag);
}

/// A page of a listing holds at most `max_keys` keys that sort after the start-after key, in
/// key order, and a page shorter than `max_keys` is the last; a page of none asks for none.
pub async fn a_page_starts_after_a_key_and_is_cut_at_max_keys(backend: &dyn Backend) {
    for key in ["a", "b", "c", "d", "x"] {
        backend.put(key, body("v")).await.unwrap();
    }
    assert_eq!(page(backend, None, 2).await, ["a", "b"]);
    assert_eq!(page(backend, Some("b"), 2).await, ["c", "d"]);
    assert_eq!(page(backend, Some("d"), 2).await, ["x"]);
    assert_eq!(page(backend, Some("x"), 2).await, Vec::<String>::new());
    assert_eq!(page(backend, None, 0).await, Vec::<String>::new());
}

/// A put with the content tag of the object replaces it and gives the new object's content
/// tag; a put with another tag is refused and changes nothing; a put over an absent key is
/// `NotFound`.
pub async fn a_put_if_match_replaces_only_the_object_with_that_tag(backend: &dyn Backend) {
    backend.put("k", body("one")).await.unwrap();
    let tag = backend.head("k").await.unwrap().tag;

    let wrong = backend.put_if_match("k", body("x"), "not-the-tag").await;
    assert_eq!(wrong, Err(Error::PreconditionFailed("k".to_owned())));
    assert_eq!(backend.get("k", None).await.unwrap(), body("one"));

    let swap = backend.put_if_match("k", body("two"), &tag);
    let new_tag = swap.await.unwrap();
    let head = backend.head("k").await.unwrap();
    assert_eq!(head.tag, new_tag);
    assert_ne!(new_tag, tag);
    assert_eq!(backend.get("k", None).await.unwrap(), body("two"));

    let stale = backend.put_if_match("k", body("three"), &tag).await;
    assert_eq!(stale, Err(Error::PreconditionFailed("k".to_owned())));

    let absent = backend.put_if_match("other", body("x"), &tag).await;
    assert_eq!(absent, Err(Error::NotFound("other".to_owned())));
}

/// A delete with the content tag of the object removes it, and the key can be put again; a
/// delete with another tag is refused and changes nothing, as is one with the tag the object had
/// before it was deleted and put again with other bytes; a delete of an absent key is
/// `NotFound`.
pub async fn a_conditional_delete_removes_only_the_object_with_that_tag(backend: &dyn Backend) {
    backend.put("k", body("one")).await.unwrap();
    let tag = backend.head("k").await.unwrap().tag;

    let wrong = backend.delete_if_match("k", "not-the-tag").await;
    assert_eq!(wrong, Err(Error::PreconditionFailed("k".to_owned())));
    assert_eq!(backend.get("k", None).await.unwrap(), body("one"));

    backend.delete_if_match("k", &tag).await.unwrap();
    assert_eq!(keys(backend).await, Vec::<String>::new());

    backend.put("k", body("two")).await.unwrap();
    let stale = backend.delete_if_match("k", &tag).await;
    assert_eq!(stale, Err(Error::PreconditionFailed("k".to_owned())));
    assert_eq!(backend.get("k", None).await.unwrap(), body("two"));

    let absent = backend.delete_if_match("other", &tag).await;
    assert_eq!(absent, Err(Error::NotFound("other".to_owned())));
}

/// A streamed put creates the object from its chunks, whose length is told or not, with the
/// size and the content tag a put of the same bytes gives.
pub async fn a_streamed_put_creates_the_object_from_its_chunks(backend: &dyn Backend) {
    backend.put("whole", body("streamed body")).await.unwrap();
    let told = chunks(&["streamed", " ", "body"]);
    backend.put_stream("told", Some(13), told).await.unwrap();
    let untold = chunks(&["streamed ", "body"]);
    backend.put_stream("untold", None, untold).await.unwrap();
    for key in ["told", "untold"] {
        assert_eq!(backend.get(key, None).await.unwrap(), body("streamed body"));
        let (made, whole) = (
            backend.head(key).await.unwrap(),
            backend.head("whole").await,
        );
        assert!(made.same_content(&whole.unwrap()), "{key}");
    }
    backend
        .put_stream("empty", Some(0), chunks(&[]))
        .await
        .unwrap();
    assert_eq!(backend.get("empty", None).await.unwrap(), body(""));
}

/// A streamed put whose body ends with an error, after chunks or at once, gives that error and
/// leaves nothing: no object under the key and none in a listing; the key can be put again.
pub async fn a_streamed_put_that_fails_leaves_nothing(backend: &dyn Backend) {
    let cut = Error::Other("cut".to_owned());
    let after_chunks = backend
        .put_stream("k", Some(9), broken(&["abc", "def"], &cut))
        .await;
    assert_eq!(after_chunks, Err(cut.clone()));
    let at_once = backend.put_stream("k", None, broken(&[], &cut)).await;
    assert_eq!(at_once, Err(cut));
    assert_eq!(keys(backend).await, Vec::<String>::new());
    let missing = Error::NotFound("k".to_owned());
    assert_eq!(backend.head("k").await.unwrap_err(), missing);
    backend
        .put_stream("k", None, chunks(&["fine"]))
        .await
        .unwrap();
    assert_eq!(backend.get("k", None).await.unwrap(), body("fine"));
}

/// A streamed put of a key that exists is refused with `AlreadyExists`, and the first object
/// stays.
pub async fn a_streamed_put_of_an_existing_key_is_refused(backend: &dyn Backend) {
    backend.put("k", body("one")).await.unwrap();
    let again = backend.put_stream("k", Some(3), chunks(&["two"])).await;
    assert_eq!(again, Err(Error::AlreadyExists("k".to_owned())));
    assert_eq!(backend.get("k", None).await.unwrap(), body("one"));
    assert_eq!(keys(backend).await, ["k"]);
}

/// A tagged streamed put creates the object from its chunks, whose length is told or not, and
/// gives the tag a head of it gives; a body that ends with an error leaves nothing, and a key that
/// exists is refused with `AlreadyExists` and keeps its object.
pub async fn a_streamed_tagged_put_gives_the_tag_of_its_object(backend: &dyn Backend) {
    let told = chunks(&["streamed", " ", "body"]);
    let first = backend.put_stream_tagged("told", Some(13), told).await;
    let untold = chunks(&["streamed ", "body"]);
    let second = backend.put_stream_tagged("untold", None, untold).await;
    for (key, tag) in [("told", first.unwrap()), ("untold", second.unwrap())] {
        assert_eq!(backend.get(key, None).await.unwrap(), body("streamed body"));
        assert_eq!(backend.head(key).await.unwrap().tag, tag, "{key}");
    }
    let cut = Error::Other("cut".to_owned());
    let failed = backend
        .put_stream_tagged("cut", Some(9), broken(&["abc"], &cut))
        .await;
    assert_eq!(failed, Err(cut));
    assert_eq!(
        backend.head("cut").await.unwrap_err(),
        Error::NotFound("cut".to_owned())
    );
    let again = backend
        .put_stream_tagged("told", Some(3), chunks(&["two"]))
        .await;
    assert_eq!(again, Err(Error::AlreadyExists("told".to_owned())));
    assert_eq!(
        backend.get("told", None).await.unwrap(),
        body("streamed body")
    );
}

/// A read with its tag gives the bytes and the tag a HEAD of the object gives, whole and by range
/// (the tag is that of the whole object whatever the range), a read of an absent key is `NotFound`
/// whole or by range, and a range that starts past the end of the object is an error.
pub async fn a_tagged_read_gives_the_bytes_and_the_tag_of_one_object(backend: &dyn Backend) {
    let held = body("0123456789");
    let tag = backend.put_tagged("k", held.clone()).await.unwrap();
    assert_eq!(backend.head("k").await.unwrap().tag, tag);
    let whole = backend.get_tagged("k", None).await.unwrap();
    assert_eq!(whole, (held.clone(), tag.clone()));
    let part = backend.get_tagged("k", Some(ByteRange::new(4, 3))).await;
    assert_eq!(part.unwrap(), (held.slice(4..7), tag));

    let missing = Error::NotFound("absent".to_owned());
    let absent = backend.get_tagged("absent", None).await;
    assert_eq!(absent, Err(missing.clone()));
    let ranged = backend
        .get_tagged("absent", Some(ByteRange::new(0, 1)))
        .await;
    assert_eq!(ranged, Err(missing));
    let past = backend.get_tagged("k", Some(ByteRange::new(11, 1))).await;
    assert!(matches!(past, Err(Error::Other(_))), "{past:?}");
}

/// The bytes a streamed read of `range` of `key` gives, joined; the error of the call that opens
/// the stream or of a chunk of it.
pub async fn streamed(
    backend: &dyn Backend,
    key: &str,
    range: Option<ByteRange>,
) -> Result<Bytes, Error> {
    let mut read = backend.get_stream(key, range).await?;
    let mut whole = Vec::new();
    while let Some(chunk) = read.next().await {
        whole.extend_from_slice(&chunk?);
    }
    Ok(Bytes::from(whole))
}

/// A streamed read gives the bytes of the object, or of the range, in order: the same bytes a get
/// gives, a range cut at the end of the object, no bytes for an empty range or one that starts at
/// the end, an error for one that starts past it, and `NotFound` for an absent key.
pub async fn a_streamed_read_gives_the_bytes_of_its_range(backend: &dyn Backend) {
    let held = body("0123456789");
    backend.put("k", held.clone()).await.unwrap();
    assert_eq!(streamed(backend, "k", None).await.unwrap(), held);
    for (offset, len, want) in [(0, 10, "0123456789"), (3, 4, "3456"), (8, 100, "89")] {
        let range = Some(ByteRange::new(offset, len));
        assert_eq!(streamed(backend, "k", range).await.unwrap(), body(want));
        assert_eq!(backend.get("k", range).await.unwrap(), body(want));
    }
    for (offset, len) in [(10, 5), (4, 0)] {
        let range = Some(ByteRange::new(offset, len));
        assert_eq!(streamed(backend, "k", range).await.unwrap(), body(""));
    }
    let past = streamed(backend, "k", Some(ByteRange::new(11, 1))).await;
    assert!(matches!(past, Err(Error::Other(_))), "{past:?}");
    let absent = streamed(backend, "absent", None).await;
    assert_eq!(absent, Err(Error::NotFound("absent".to_owned())));
    backend.put("empty", body("")).await.unwrap();
    assert_eq!(streamed(backend, "empty", None).await.unwrap(), body(""));
}

/// A streamed replacement replaces an object only if it has the tag and the body ended
/// whole: another tag and a body that fails each leave the object as it was; the tag gives
/// a new object, with the tag the replacement returned, whether the length
/// is told or not; the old tag is then refused, an absent key is `NotFound`, and nothing else is
/// left in the backend.
pub async fn a_streamed_replace_takes_the_tag_and_a_whole_body(backend: &dyn Backend) {
    let first = body("original");
    let old = backend.put_tagged("k", first.clone()).await.unwrap();
    let wrong = backend.put_stream_if_match("k", Some(8), chunks(&["abcd", "efgh"]), "not-the-tag");
    assert_eq!(wrong.await, Err(Error::PreconditionFailed("k".to_owned())));
    let cut = Error::Other("cut".to_owned());
    let failing = broken(&["abcd"], &cut);
    let failed = backend
        .put_stream_if_match("k", Some(8), failing, &old)
        .await;
    assert_eq!(failed, Err(cut));
    let kept = backend.get_tagged("k", None).await.unwrap();
    assert_eq!(kept, (first, old.clone()));
    assert_eq!(backend.head("k").await.unwrap().tag, old);

    let told = backend.put_stream_if_match("k", Some(8), chunks(&["abcd", "efgh"]), &old);
    let new = told.await.unwrap();
    let head = backend.head("k").await.unwrap();
    assert_eq!((head.tag.as_str(), head.size), (new.as_str(), 8));
    assert_eq!(backend.get("k", None).await.unwrap(), body("abcdefgh"));
    let untold = backend.put_stream_if_match("k", None, chunks(&["12", "34"]), &new);
    let newer = untold.await.unwrap();
    assert_eq!(backend.head("k").await.unwrap().tag, newer);
    assert_eq!(backend.get("k", None).await.unwrap(), body("1234"));

    let stale = backend
        .put_stream_if_match("k", Some(1), chunks(&["x"]), &old)
        .await;
    assert_eq!(stale, Err(Error::PreconditionFailed("k".to_owned())));
    let absent = backend.put_stream_if_match("none", Some(1), chunks(&["x"]), &old);
    assert_eq!(absent.await, Err(Error::NotFound("none".to_owned())));
    assert_eq!(keys(backend).await, ["k"]);
}

/// The bytes of the body of an open read, joined.
pub async fn joined(opened: Opened) -> Bytes {
    let mut body = opened.body;
    let mut whole = Vec::new();
    while let Some(chunk) = body.next().await {
        whole.extend_from_slice(&chunk.unwrap());
    }
    Bytes::from(whole)
}

/// An open read gives the meta a HEAD gives (the size, the tag and the time) and, as a
/// body, the bytes of its span, with the window it resolved the span to: all of the object
/// without a span; a range cut at the end of the object; the last bytes of it (all of it, if it
/// is shorter than asked); no bytes and the unsatisfiable window for a span that starts at or
/// past the end, for the last none of it and for any span of an empty object; `NotFound` for an
/// absent key.
pub async fn an_open_read_gives_the_meta_and_the_bytes_of_its_span(backend: &dyn Backend) {
    backend.put("k", body("0123456789")).await.unwrap();
    let head = backend.head("k").await.unwrap();
    let from = |first, last| Some(Span::From { first, last });
    let part = |offset, len| Window::Part(ByteRange::new(offset, len));
    let cases = [
        (None, Window::Whole, "0123456789"),
        (from(3, Some(5)), part(3, 3), "345"),
        (from(0, Some(0)), part(0, 1), "0"),
        (from(2, None), part(2, 8), "23456789"),
        (from(8, Some(100)), part(8, 2), "89"),
        (Some(Span::Suffix(4)), part(6, 4), "6789"),
        (Some(Span::Suffix(40)), part(0, 10), "0123456789"),
        (from(10, None), Window::Unsatisfiable, ""),
        (from(u64::MAX, None), Window::Unsatisfiable, ""),
        (Some(Span::Suffix(0)), Window::Unsatisfiable, ""),
    ];
    for (span, window, want) in cases {
        let opened = backend.open_read("k", span).await.unwrap();
        assert_eq!(opened.meta, head, "{span:?}");
        assert_eq!(opened.window, window, "{span:?}");
        assert_eq!(joined(opened).await, body(want), "{span:?}");
    }

    backend.put("empty", body("")).await.unwrap();
    for span in [None, from(0, None), Some(Span::Suffix(1))] {
        let opened = backend.open_read("empty", span).await.unwrap();
        assert_eq!(opened.meta.size, 0, "{span:?}");
        let window = if span.is_some() {
            Window::Unsatisfiable
        } else {
            Window::Whole
        };
        assert_eq!(opened.window, window, "{span:?}");
        assert_eq!(joined(opened).await, body(""), "{span:?}");
    }

    let absent = backend.open_read("absent", None).await;
    assert_eq!(absent.err(), Some(Error::NotFound("absent".to_owned())));
}
