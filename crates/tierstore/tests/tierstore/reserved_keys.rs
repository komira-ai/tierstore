//! The key space the proxy keeps for itself: a key under `_entries/`, `_journal/` or `_tiering/`
//! is refused by every call that names one object, and any other key, `_uploads/x` and
//! `_entries` (no slash) among them, is an ordinary key that every call takes.

use bytes::Bytes;
use futures::StreamExt as _;
use tierstore::{Backend, ByteRange, Error, ReadBody, Span};

use crate::support::{Rig, assert_absent, body, chunks, keys, keys_under, page, read};

/// Keys beside the reserved prefixes that are not under one: another prefix that starts with `_`,
/// a reserved name with no slash, a reserved name with more after it, a reserved name in another
/// case, and a reserved prefix that is not at the start of the key.
const ORDINARY: [&str; 8] = [
    "_uploads/x",
    "_entries",
    "_journal",
    "_tiering",
    "_tieringx/1",
    "_entries_/y",
    "_Tiering/x",
    "x/_tiering/y",
];

/// The bytes of a streamed body, read to its end.
async fn drained(mut stream: ReadBody) -> Bytes {
    let mut all = Vec::new();
    while let Some(chunk) = stream.next().await {
        all.extend_from_slice(&chunk.unwrap());
    }
    Bytes::from(all)
}

/// Asserts that a put, a get and a listing of `_uploads/x` through the proxy succeed and the key is
/// listed, while a put of `_tiering/x` is refused with `Reserved` and writes to neither tier.
#[tokio::test(start_paused = true)]
async fn key_under_uploads_prefix_is_an_ordinary_key() {
    let rig = Rig::new();
    let proxy = rig.proxy("p1", 1000).await;

    proxy.put("_uploads/x", body("value")).await.unwrap();
    assert_eq!(proxy.get("_uploads/x", None).await.unwrap(), body("value"));
    assert_eq!(keys(&proxy).await, ["_uploads/x"]);
    assert_eq!(keys_under(&proxy, "_uploads/").await, ["_uploads/x"]);
    assert_eq!(page(&proxy, None, 10).await, ["_uploads/x"]);

    let refused = proxy.put("_tiering/x", body("value")).await;
    assert_eq!(refused, Err(Error::Reserved("_tiering/x".to_owned())));
    assert_absent(&*rig.hot_mem, "_tiering/x").await;
    assert_absent(&*rig.cold_mem, "_tiering/x").await;
}

/// Asserts that every key of [`ORDINARY`] takes a put (held or streamed), a head, a get (whole and
/// of a range), a streamed and an open read, a listing (whole, by prefix and by pages, with the
/// proxy's own entry objects of the same puts sorted in between and left out) and a delete.
#[tokio::test(start_paused = true)]
async fn keys_beside_the_reserved_prefixes_are_ordinary_keys_for_every_call() {
    let rig = Rig::new();
    let proxy = rig.proxy("p1", 1000).await;
    for (at, key) in ORDINARY.iter().enumerate() {
        if at % 2 == 0 {
            proxy.put(key, body("value")).await.unwrap();
        } else {
            let streamed = proxy.put_stream(key, Some(5), chunks(&["val", "ue"]));
            streamed.await.unwrap();
        }
    }

    for key in ORDINARY {
        assert_eq!(proxy.head(key).await.unwrap().size, 5, "head {key}");
        assert_eq!(read(&proxy, key).await, body("value"), "get {key}");
        let range = Some(ByteRange::new(1, 3));
        assert_eq!(proxy.get(key, range).await.unwrap(), body("alu"), "{key}");
        let streamed = proxy.get_stream(key, None).await.unwrap();
        assert_eq!(drained(streamed).await, body("value"), "stream {key}");
        let span = Some(Span::From {
            first: 3,
            last: None,
        });
        let opened = proxy.open_read(key, span).await.unwrap();
        assert_eq!(drained(opened.body).await, body("ue"), "open {key}");
    }

    let mut sorted = ORDINARY.to_vec();
    sorted.sort_unstable();
    assert_eq!(keys(&proxy).await, sorted);
    // `_entries` is a prefix of the entry objects `_entries/<digest>` of these puts, which sort
    // between `_entries` and `_entries_/y`.
    assert_eq!(
        keys_under(&proxy, "_entries").await,
        ["_entries", "_entries_/y"]
    );
    assert_eq!(
        keys_under(&proxy, "_tiering").await,
        ["_tiering", "_tieringx/1"]
    );
    let mut paged = Vec::new();
    loop {
        let next = page(&proxy, paged.last().map(String::as_str), 2).await;
        if next.is_empty() {
            break;
        }
        paged.extend(next);
    }
    assert_eq!(paged, sorted);

    for key in ORDINARY {
        proxy.delete(key).await.unwrap();
        assert_absent(&proxy, key).await;
    }
    assert!(keys(&proxy).await.is_empty());
}

/// Asserts that a key under each reserved prefix is refused with `Reserved` by a put (held and
/// streamed), a head, a get (whole and of a range), a streamed read (whole and of a range), an
/// open read and a delete, whether the hot tier holds it, the cold tier holds it, or neither, and
/// that the objects the tiers hold are left as they are and nothing is written.
#[tokio::test(start_paused = true)]
async fn a_key_under_each_reserved_prefix_is_refused_by_every_call() {
    let rig = Rig::new();
    let proxy = rig.proxy("p1", 1000).await;
    rig.hot_mem.put("_entries/h", body("hot")).await.unwrap();
    rig.cold_mem.put("_journal/c", body("cold")).await.unwrap();
    rig.cold_mem.put("_tiering/c", body("cold")).await.unwrap();
    for key in [
        "_entries/h",
        "_journal/c",
        "_tiering/c",
        "_entries/n",
        "_journal/n",
        "_tiering/n",
    ] {
        let refused = Some(Error::Reserved(key.to_owned()));
        assert_eq!(
            proxy.put(key, body("new")).await.err(),
            refused,
            "put {key}"
        );
        let streamed = proxy.put_stream(key, Some(3), chunks(&["n", "ew"])).await;
        assert_eq!(streamed.err(), refused, "put_stream {key}");
        assert_eq!(proxy.head(key).await.err(), refused, "head {key}");
        assert_eq!(proxy.get(key, None).await.err(), refused, "get {key}");
        let range = Some(ByteRange::new(0, 1));
        assert_eq!(proxy.get(key, range).await.err(), refused, "range {key}");
        let whole = proxy.get_stream(key, None).await;
        assert_eq!(whole.err(), refused, "get_stream {key}");
        let part = proxy.get_stream(key, range).await;
        assert_eq!(part.err(), refused, "get_stream range {key}");
        let opened = proxy.open_read(key, None).await;
        assert_eq!(opened.err(), refused, "open_read {key}");
        assert_eq!(proxy.delete(key).await.err(), refused, "delete {key}");
    }
    assert_eq!(read(&*rig.hot_mem, "_entries/h").await, body("hot"));
    assert_eq!(read(&*rig.cold_mem, "_journal/c").await, body("cold"));
    assert_eq!(read(&*rig.cold_mem, "_tiering/c").await, body("cold"));
    for key in ["_entries/n", "_journal/n", "_tiering/n"] {
        assert_absent(&*rig.hot_mem, key).await;
        assert_absent(&*rig.cold_mem, key).await;
    }
}
